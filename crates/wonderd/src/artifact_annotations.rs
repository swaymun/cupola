//! Versioned preview notes ride the existing immutable attachment and message
//! intent path. The attachment is JSON, but the provider receives its note as
//! text; merely handing the provider the JSON file path would lose the note.
mod diff;

use crate::{attachments, projects, AppState};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Component, Path};
use tokio::io::AsyncReadExt;
use wonder_store::{StoredConversationFile, StoredProject};

pub(crate) const MIME: &str = wonder_store::ARTIFACT_ANNOTATION_MIME;
const MAX_RECORD_BYTES: usize = 16 * 1024;
const MAX_NOTE_BYTES: usize = 4096;
const MAX_TEXT_EXCERPT_BYTES: usize = 2048;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PreviewAnnotation {
    pub version: u8,
    pub project_id: String,
    pub conversation_id: String,
    pub root_id: String,
    pub path: String,
    pub source_sha256: String,
    pub anchor: PreviewAnchor,
    pub note: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(crate) enum PreviewAnchor {
    TextLines {
        start_line: u32,
        end_line: u32,
    },
    DiffLines {
        scope: String,
        original_path: Option<String>,
        start_line: u32,
        end_line: u32,
    },
    TextRange {
        start_byte: usize,
        end_byte: usize,
    },
    ImageRegion {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
    PdfRegion {
        page: u32,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
}

fn invalid(detail: &str) -> (StatusCode, String) {
    (StatusCode::UNPROCESSABLE_ENTITY, detail.to_owned())
}

fn valid_region(x: f64, y: f64, width: f64, height: f64) -> bool {
    [x, y, width, height].into_iter().all(f64::is_finite)
        && x >= 0.0
        && y >= 0.0
        && width > 0.0
        && height > 0.0
        && x + width <= 1.0
        && y + height <= 1.0
}

fn text_format(mime: &str) -> bool {
    (mime.starts_with("text/") && mime != "text/html")
        || matches!(
            mime,
            "application/json" | "application/yaml" | "application/xml" | "application/sql"
        )
}

#[cfg(target_os = "macos")]
fn valid_pdf_page(bytes: &[u8], page: u32) -> bool {
    use std::ffi::c_void;
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGDataProviderCreateWithData(
            info: *mut c_void,
            data: *const c_void,
            size: usize,
            release_data: Option<unsafe extern "C" fn(*mut c_void, *const c_void, usize)>,
        ) -> *mut c_void;
        fn CGDataProviderRelease(provider: *mut c_void);
        fn CGPDFDocumentCreateWithProvider(provider: *mut c_void) -> *mut c_void;
        fn CGPDFDocumentGetNumberOfPages(document: *mut c_void) -> usize;
        fn CGPDFDocumentGetPage(document: *mut c_void, page: usize) -> *mut c_void;
        fn CGPDFDocumentRelease(document: *mut c_void);
    }
    // Core Graphics borrows these bounded bytes until both objects are
    // released below. Keep parsing on the blocking worker, never an async
    // executor or SwiftUI thread.
    unsafe {
        let provider = CGDataProviderCreateWithData(
            std::ptr::null_mut(),
            bytes.as_ptr().cast(),
            bytes.len(),
            None,
        );
        if provider.is_null() {
            return false;
        }
        let document = CGPDFDocumentCreateWithProvider(provider);
        if document.is_null() {
            CGDataProviderRelease(provider);
            return false;
        }
        let valid = page > 0
            && page as usize <= CGPDFDocumentGetNumberOfPages(document)
            && !CGPDFDocumentGetPage(document, page as usize).is_null();
        CGPDFDocumentRelease(document);
        CGDataProviderRelease(provider);
        valid
    }
}

#[cfg(not(target_os = "macos"))]
fn valid_pdf_page(_bytes: &[u8], _page: u32) -> bool {
    // The Wonder host supports PDF region validation on its Mac companion.
    false
}

fn validate_record(
    record: &PreviewAnnotation,
    mime: &str,
    bytes: &[u8],
) -> Result<(), (StatusCode, String)> {
    if record.version != 1
        || record.root_id.is_empty()
        || record.root_id.len() > 128
        || record.path.is_empty()
        || record.path.len() > 4096
        || record.source_sha256.len() != 64
        || !record
            .source_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || record.note.trim().is_empty()
        || record.note.len() > MAX_NOTE_BYTES
        || record
            .note
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(invalid(
            "This annotation has invalid content. Edit or remove it.",
        ));
    }
    let valid_anchor = match &record.anchor {
        PreviewAnchor::TextLines {
            start_line,
            end_line,
        } => {
            let lines = std::str::from_utf8(bytes)
                .map(|text| text.lines().count().max(1))
                .unwrap_or(0);
            text_format(mime)
                && *start_line > 0
                && *end_line >= *start_line
                && *end_line as usize <= lines
        }
        PreviewAnchor::DiffLines { .. } => {
            mime == "text/x-diff" && diff::selection(&record.anchor, bytes).is_ok()
        }
        PreviewAnchor::TextRange {
            start_byte,
            end_byte,
        } => {
            text_format(mime)
                && *end_byte > *start_byte
                && std::str::from_utf8(bytes)
                    .ok()
                    .and_then(|text| text.get(*start_byte..*end_byte))
                    .is_some()
        }
        PreviewAnchor::ImageRegion {
            x,
            y,
            width,
            height,
        } => mime.starts_with("image/") && valid_region(*x, *y, *width, *height),
        PreviewAnchor::PdfRegion {
            page,
            x,
            y,
            width,
            height,
        } => {
            mime == "application/pdf"
                && *page > 0
                && *page <= 10_000
                && valid_pdf_page(bytes, *page)
                && valid_region(*x, *y, *width, *height)
        }
    };
    if !valid_anchor {
        return Err(invalid(
            "This annotation no longer fits the preview. Select its region again.",
        ));
    }
    Ok(())
}

fn parse_record(
    bytes: &[u8],
    project_id: &str,
    conversation_id: &str,
) -> Result<PreviewAnnotation, (StatusCode, String)> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(invalid("This annotation is too large. Shorten its note."));
    }
    let record: PreviewAnnotation = serde_json::from_slice(bytes)
        .map_err(|_| invalid("This annotation could not be read. Remove it and try again."))?;
    if record.project_id != project_id || record.conversation_id != conversation_id {
        return Err((
            StatusCode::FORBIDDEN,
            "This annotation belongs to another Project chat.".into(),
        ));
    }
    if record.version != 1 {
        return Err(invalid(
            "This annotation version is unsupported. Remove it and try again.",
        ));
    }
    Ok(record)
}

fn source_location(
    record: &PreviewAnnotation,
    project: &StoredProject,
    cwd: &str,
) -> Result<String, (StatusCode, String)> {
    let root = if record.root_id == "workspace" {
        project.root_for(cwd)
    } else {
        project
            .roots
            .iter()
            .find(|root| record.root_id == format!("project-{}", root.id))
    }
    .ok_or((
        StatusCode::CONFLICT,
        "An annotated Project folder changed. Reopen the preview before sending.".into(),
    ))?;
    let relative = Path::new(&record.path);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || record.path.contains('\0')
        || !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(invalid("This annotation has an invalid file path."));
    }
    Path::new(&root.canonical_path)
        .join(relative)
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("This annotation's file path cannot be read."))
}

fn prompt_text(
    record: &PreviewAnnotation,
    snapshot_path: &str,
    source: &[u8],
) -> Result<String, (StatusCode, String)> {
    let anchor = match &record.anchor {
        PreviewAnchor::TextLines {
            start_line,
            end_line,
        } => format!("lines {start_line}-{end_line}"),
        PreviewAnchor::DiffLines { scope, original_path, start_line, end_line } => format!(
            "displayed {scope} unified diff rows {start_line}-{end_line}; old path: {}. Minus rows refer to the old side, plus rows to the new side; these are not current-file anchors",
            original_path.as_deref().unwrap_or(&record.path)),
        PreviewAnchor::TextRange {
            start_byte,
            end_byte,
        } => format!("UTF-8 bytes {start_byte}..{end_byte} (end exclusive)"),
        PreviewAnchor::ImageRegion {
            x,
            y,
            width,
            height,
        } => format!("image region (normalized 0-1, origin at top left) x={x}, y={y}, width={width}, height={height}"),
        PreviewAnchor::PdfRegion {
            page,
            x,
            y,
            width,
            height,
        } => format!("PDF page {page}, region (normalized 0-1, origin at top left) x={x}, y={y}, width={width}, height={height}"),
    };
    let is_diff = matches!(record.anchor, PreviewAnchor::DiffLines { .. });
    let location_label = if is_diff {
        "Diff path"
    } else {
        "Original file"
    };
    let copy_label = if is_diff {
        "Verified displayed diff copy (unified patch, not current file contents)"
    } else {
        "Verified source copy"
    };
    let mut prompt = format!(
        "Artifact annotation (v1)\nProject: {}\n{location_label}: {}/{}\n{copy_label}: {}\nSource SHA-256: {}\nAnchor: {}\nNote:\n{}",
        record.project_id, record.root_id, record.path, snapshot_path,
        record.source_sha256, anchor, record.note
    );
    if matches!(record.anchor, PreviewAnchor::DiffLines { .. }) {
        prompt.push_str("\nSelected diff rows (source text as JSON strings):\n");
        prompt.push_str(&diff::selection(&record.anchor, source)?);
    }
    if let PreviewAnchor::TextRange {
        start_byte,
        end_byte,
    } = &record.anchor
    {
        let text = std::str::from_utf8(source)
            .ok()
            .and_then(|text| text.get(*start_byte..*end_byte))
            .ok_or_else(|| invalid("This annotation no longer fits the verified source copy."))?;
        let mut limit = text.len().min(MAX_TEXT_EXCERPT_BYTES);
        while !text.is_char_boundary(limit) {
            limit -= 1;
        }
        let excerpt = serde_json::to_string(&text[..limit])
            .map_err(|_| invalid("This annotation's selected text could not be read."))?;
        prompt.push_str("\nSelected source text excerpt (JSON string):\n");
        prompt.push_str(&excerpt);
        if limit < text.len() {
            prompt.push_str(
                "\n[Excerpt truncated; use the verified source copy for the full selection.]",
            );
        }
    }
    Ok(prompt)
}

async fn source_copy_path(
    workspace: &str,
    annotation_id: &str,
    create: bool,
) -> Option<std::path::PathBuf> {
    let copy_id = crate::deterministic_uuid(&format!("artifact-source:{annotation_id}"));
    crate::attachment_path(workspace, &copy_id, create).await
}

async fn verified_source_copy(path: &Path, hash: &str) -> Result<Vec<u8>, (StatusCode, String)> {
    let file = tokio::fs::File::open(path).await.map_err(|_| {
        (
            StatusCode::CONFLICT,
            "The annotated source copy is unavailable.".into(),
        )
    })?;
    let mut bytes = Vec::new();
    file.take(crate::MAX_ATTACHMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| {
            (
                StatusCode::CONFLICT,
                "The annotated source copy could not be read.".into(),
            )
        })?;
    if bytes.len() > crate::MAX_ATTACHMENT_BYTES || hex::encode(Sha256::digest(&bytes)) != hash {
        return Err((
            StatusCode::CONFLICT,
            "The annotated source copy changed. Reopen the preview and send a new note.".into(),
        ));
    }
    Ok(bytes)
}

/// Checks selected annotation attachment bytes against the current Project
/// source before first acceptance. On dispatch, the same frozen bytes become
/// one provider text input per message attachment without reopening a stale
/// source that may have changed after the user sent the message.
pub(crate) async fn selected_inputs(
    state: &AppState,
    conversation_id: &str,
    files: &[StoredConversationFile],
    verify_source: bool,
) -> Result<Vec<Value>, (StatusCode, String)> {
    let annotations = files
        .iter()
        .filter(|file| file.mime_type.as_deref() == Some(MIME))
        .collect::<Vec<_>>();
    if annotations.is_empty() {
        return Ok(Vec::new());
    }
    let conversation = state
        .store
        .project_conversation(conversation_id)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The Project could not be checked.".into(),
            )
        })?
        .ok_or((
            StatusCode::FORBIDDEN,
            "Annotations require a Project chat.".into(),
        ))?;
    // First acceptance binds the note to the current source and Project roots.
    // An accepted message already owns a verified immutable source copy; root
    // edits must not strand its provider dispatch or redirect it to live bytes.
    let project = if verify_source {
        let project = state
            .store
            .project(&conversation.project_id)
            .await
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The Project could not be checked.".into(),
                )
            })?
            .ok_or((StatusCode::NOT_FOUND, "This Project was removed.".into()))?;
        let project = projects::thread_project(state, project, &conversation.cwd).await;
        if conversation.roots_revision != project.roots_revision {
            return Err((
                StatusCode::CONFLICT,
                "Project folders changed. Reopen the annotation preview before sending.".into(),
            ));
        }
        Some(project)
    } else {
        None
    };
    let workspace = projects::media_workspace(state, conversation_id)
        .await
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            "The annotation storage is unavailable.".into(),
        ))?;
    let mut inputs = Vec::with_capacity(annotations.len());
    for file in annotations {
        if file.conversation_id != conversation_id || file.kind != "attachment" {
            return Err((
                StatusCode::FORBIDDEN,
                "This annotation belongs to another chat.".into(),
            ));
        }
        let path = crate::attachment_path(&workspace, &file.id, false)
            .await
            .ok_or((
                StatusCode::CONFLICT,
                "This annotation is unavailable. Attach it again.".into(),
            ))?;
        let bytes = attachments::verified_bytes(&path, file)
            .await
            .map_err(|_| {
                (
                    StatusCode::CONFLICT,
                    "This annotation changed. Attach it again.".into(),
                )
            })?;
        let record = parse_record(&bytes, &conversation.project_id, conversation_id)?;
        // At first acceptance, ensure the note names a folder in this chat.
        // Dispatch uses the frozen copy, never a mutable source or root map.
        if let Some(project) = &project {
            let _source_path = source_location(&record, project, &conversation.cwd)?;
        }
        let copy = source_copy_path(&workspace, &file.id, verify_source)
            .await
            .ok_or((
                StatusCode::CONFLICT,
                "The annotated source copy is unavailable.".into(),
            ))?;
        if verify_source {
            let (source, mime) = if let PreviewAnchor::DiffLines {
                scope,
                original_path,
                ..
            } = &record.anchor
            {
                (
                    crate::filesystem::verified_project_diff(
                        state,
                        conversation_id,
                        &record.root_id,
                        &record.path,
                        scope,
                        original_path.as_deref(),
                    )
                    .await?,
                    "text/x-diff",
                )
            } else {
                crate::filesystem::verified_project_preview_file(
                    state,
                    &conversation.project_id,
                    conversation_id,
                    &record.root_id,
                    &record.path,
                )
                .await?
            };
            let hash = hex::encode(Sha256::digest(&source));
            if hash != record.source_sha256 {
                return Err((StatusCode::CONFLICT, if matches!(record.anchor, PreviewAnchor::DiffLines { .. }) {
                    "This diff changed since you commented. Reopen Code changes and select its lines again."
                } else { "This file changed since you annotated it. Reopen the preview and update the annotation." }.into()));
            }
            let record_for_validation = record.clone();
            let source = tokio::task::spawn_blocking(move || {
                validate_record(&record_for_validation, mime, &source)?;
                Ok::<_, (StatusCode, String)>(source)
            })
            .await
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The annotation could not be checked. Try again.".into(),
                )
            })??;
            crate::attachments::write_immutable(&copy, &source)
                .await
                .map_err(|_| {
                    (
                        StatusCode::CONFLICT,
                        "The annotated source could not be saved. Try again.".into(),
                    )
                })?;
        }
        let source = verified_source_copy(&copy, &record.source_sha256).await?;
        inputs.push(json!({"type":"text", "text":prompt_text(&record, copy.to_str().ok_or_else(|| invalid("The source copy path cannot be read."))?, &source)?}));
    }
    Ok(inputs)
}

/// Both an existing Project chat and a prepared first Project message must
/// pass the same source check before accepting a new message intent. An exact
/// accepted retry keeps its frozen attachment even if the source later moves.
pub(crate) async fn validate_first_acceptance(
    state: &AppState,
    conversation_id: &str,
    device_id: &str,
    client_message_id: &str,
    attachment_ids: &[String],
) -> Result<(), (StatusCode, String)> {
    if attachment_ids.is_empty() {
        return Ok(());
    }
    let accepted = state
        .store
        .message_by_device_and_client_message_id(device_id, client_message_id)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The message could not be checked. Try again.".into(),
            )
        })?;
    if accepted.is_some() {
        return Ok(());
    }
    let files = state
        .store
        .conversation_files_by_ids(conversation_id, attachment_ids)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The attachments could not be checked. Try again.".into(),
            )
        })?;
    selected_inputs(state, conversation_id, &files, true).await?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axum::{
        extract::{Path, State},
        Json,
    };
    use base64::Engine as _;
    use wonder_store::{AgentFamily, ProjectConversationInsert, ProjectRootInput};

    #[test]
    fn selected_text_range_is_bound_to_utf8_source_and_bounded_in_provider_prompt() {
        let source = "Aé🦊Z\n".as_bytes();
        let mut record = PreviewAnnotation {
            version: 1,
            project_id: "project".into(),
            conversation_id: "chat".into(),
            root_id: "workspace".into(),
            path: "notes.md".into(),
            source_sha256: hex::encode(Sha256::digest(source)),
            anchor: PreviewAnchor::TextRange {
                start_byte: 1,
                end_byte: 7,
            },
            note: "Inspect this exact phrase".into(),
        };
        let wire = serde_json::to_value(&record).unwrap();
        crate::tests::validate_http_contract("artifactAnnotation", &wire);
        assert_eq!(
            wire["anchor"],
            json!({"kind":"textRange","startByte":1,"endByte":7})
        );
        assert!(validate_record(&record, "text/markdown", source).is_ok());
        let prompt = prompt_text(&record, "/verified/copy", source).unwrap();
        assert!(prompt.contains("UTF-8 bytes 1..7 (end exclusive)"));
        assert!(prompt.contains("\"é🦊\""));

        assert!(validate_record(&record, "text/html", source).is_err());
        assert!(validate_record(&record, "image/png", source).is_err());
        record.anchor = PreviewAnchor::TextRange {
            start_byte: 2,
            end_byte: 7,
        };
        assert!(validate_record(&record, "text/markdown", source).is_err());
        record.anchor = PreviewAnchor::TextRange {
            start_byte: 1,
            end_byte: 6,
        };
        assert!(validate_record(&record, "text/markdown", source).is_err());
        record.anchor = PreviewAnchor::TextRange {
            start_byte: 7,
            end_byte: 7,
        };
        assert!(validate_record(&record, "text/markdown", source).is_err());
        record.anchor = PreviewAnchor::TextRange {
            start_byte: 0,
            end_byte: MAX_TEXT_EXCERPT_BYTES + 500,
        };
        assert!(validate_record(&record, "text/markdown", source).is_err());
        let long_source = "é".repeat(MAX_TEXT_EXCERPT_BYTES);
        record.anchor = PreviewAnchor::TextRange {
            start_byte: 0,
            end_byte: long_source.len(),
        };
        record.source_sha256 = hex::encode(Sha256::digest(long_source.as_bytes()));
        assert!(validate_record(&record, "text/plain", long_source.as_bytes()).is_ok());
        let prompt = prompt_text(&record, "/verified/copy", long_source.as_bytes()).unwrap();
        assert!(prompt.contains("[Excerpt truncated;"));
        assert!(!prompt.contains(&"é".repeat(MAX_TEXT_EXCERPT_BYTES)));

        // A patch anchor must identify real content rows in a complete hunk,
        // and cannot be interpreted as current-file text or cross metadata.
        let patch = b"diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,1 +1,2 @@\n-old\n+new\n+second\n";
        record.source_sha256 = hex::encode(Sha256::digest(patch));
        for (start, end, valid) in [
            (5, 5, true),
            (6, 7, true),
            (4, 5, false),
            (7, 8, false),
            (0, 1, false),
            (5, 105, false),
        ] {
            record.anchor = PreviewAnchor::DiffLines {
                scope: "uncommitted".into(),
                original_path: None,
                start_line: start,
                end_line: end,
            };
            assert_eq!(
                validate_record(&record, "text/x-diff", patch).is_ok(),
                valid,
                "{start}..{end}"
            );
            assert!(validate_record(&record, "text/plain", patch).is_err());
        }
        record.anchor = PreviewAnchor::DiffLines {
            scope: "branch".into(),
            original_path: None,
            start_line: 5,
            end_line: 5,
        };
        assert!(validate_record(&record, "text/x-diff", &patch[..patch.len() - 8]).is_err());
        assert!(
            validate_record(&record, "text/x-diff", b"Binary files a/x and b/x differ\n").is_err()
        );
    }

    #[tokio::test]
    async fn annotation_is_bound_to_current_source_and_durable_message_selection() {
        for side in [None, Some("old"), Some("new")] {
            bound_source_and_message(side).await;
        }
    }

    async fn bound_source_and_message(diff_side: Option<&str>) {
        let diff_note = diff_side.is_some();
        let expected_excerpt = match diff_side {
            Some("old") => "\"-second line\"",
            Some(_) => "\"+replacement line\"",
            None => "\"second line\"",
        };
        let (fixture, mut state) = crate::ingestion::tests::fixture().await;
        let projects = fixture.path().join("projects");
        std::fs::create_dir(&projects).unwrap();
        state.data_dir = projects.parent().unwrap().to_path_buf();
        let source = fixture.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let secondary = fixture.path().join("secondary");
        std::fs::create_dir(&secondary).unwrap();
        let source_file = source.join("note.txt");
        std::fs::write(&source_file, b"first line\nsecond line\n").unwrap();
        let roots = [&source, &secondary]
            .into_iter()
            .map(|path| ProjectRootInput {
                path: path.to_string_lossy().into_owned(),
                canonical_path: crate::canonical_path::canonicalize(path)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            })
            .collect::<Vec<_>>();
        let project = state
            .store
            .create_project("p", "request", "hash", "Test", &roots, 0, "now")
            .await
            .unwrap();
        let wonder_store::ProjectCreate::Created(project) = project else {
            panic!("project created")
        };
        let conversation = uuid::Uuid::new_v4().to_string();
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: &conversation,
                project_id: &project.id,
                family: AgentFamily::Codex,
                provider_store: &state.projects.codex_store,
                native_session_id: None,
                cwd: source.to_str().unwrap(),
                roots_revision: project.roots_revision,
                title: "Draft",
                model: None,
                effort: None,
                service_tier: None,
                access_mode: "read_only",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
        let source_bytes = if diff_note {
            let git = |args: &[&str]| {
                let result = crate::test_git::git()
                    .arg("-C")
                    .arg(&source)
                    .args([
                        "-c",
                        "user.name=Fixture",
                        "-c",
                        "user.email=fixture@example.invalid",
                        "-c",
                        "core.hooksPath=/dev/null",
                    ])
                    .args(args)
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
            };
            git(&["init", "-q"]);
            git(&["add", "note.txt"]);
            git(&["commit", "-qm", "before"]);
            std::fs::write(&source_file, b"first line\nreplacement line\n").unwrap();
            crate::filesystem::verified_project_diff(
                &state,
                &conversation,
                "workspace",
                "note.txt",
                "uncommitted",
                None,
            )
            .await
            .unwrap()
        } else {
            std::fs::read(&source_file).unwrap()
        };
        let selected_row = std::str::from_utf8(&source_bytes)
            .unwrap()
            .lines()
            .position(|line| {
                line == if diff_side == Some("old") {
                    "-second line"
                } else {
                    "+replacement line"
                }
            })
            .map(|index| index as u32 + 1);
        let record = PreviewAnnotation {
            version: 1,
            project_id: project.id.clone(),
            conversation_id: conversation.clone(),
            root_id: "workspace".into(),
            path: "note.txt".into(),
            source_sha256: hex::encode(Sha256::digest(&source_bytes)),
            anchor: if let Some(row) = selected_row {
                PreviewAnchor::DiffLines {
                    scope: "uncommitted".into(),
                    original_path: None,
                    start_line: row,
                    end_line: row,
                }
            } else {
                PreviewAnchor::TextRange {
                    start_byte: 11,
                    end_byte: 22,
                }
            },
            note: "Check this line\nOriginal file: retain these words; Verified source copy: also retain".into(),
        };
        let secondary_root = project
            .roots
            .iter()
            .find(|root| root.path == secondary.to_str().unwrap())
            .unwrap();
        let mut secondary_record = record.clone();
        secondary_record.root_id = format!("project-{}", secondary_root.id);
        secondary_record.path = "another.txt".into();
        assert_eq!(
            source_location(&secondary_record, &project, source.to_str().unwrap()).unwrap(),
            std::path::Path::new(&secondary_root.canonical_path)
                .join("another.txt")
                .to_str()
                .unwrap()
        );
        secondary_record.path = "../outside.txt".into();
        assert_eq!(
            source_location(&secondary_record, &project, source.to_str().unwrap())
                .unwrap_err()
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        crate::tests::validate_http_contract(
            "artifactAnnotation",
            &serde_json::to_value(&record).unwrap(),
        );
        let response = crate::upload_conversation_file(
            State(state.clone()),
            Path(conversation.clone()),
            Json(crate::CreateConversationFileRequest {
                client_upload_id: Some(uuid::Uuid::new_v4().to_string()),
                name: "Preview annotation.json".into(),
                mime_type: Some(MIME.into()),
                content_base64: base64::engine::general_purpose::STANDARD
                    .encode(serde_json::to_vec(&record).unwrap()),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let upload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap(),
        )
        .unwrap();
        let file_id = upload["id"].as_str().unwrap().to_owned();
        let selected = state
            .store
            .list_conversation_files(&conversation)
            .await
            .unwrap()
            .into_iter()
            .filter(|file| file.id == file_id)
            .collect::<Vec<_>>();
        assert_eq!(
            parse_record(
                &serde_json::to_vec(&record).unwrap(),
                "other-project",
                &conversation
            )
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
        let input = selected_inputs(&state, &conversation, &selected, true)
            .await
            .unwrap();
        assert_eq!(input.len(), 1);
        assert!(input[0]["text"].as_str().unwrap().contains(&record.note));
        assert!(input[0]["text"]
            .as_str()
            .unwrap()
            .contains("Check this line"));
        assert!(input[0]["text"]
            .as_str()
            .unwrap()
            .contains(expected_excerpt));
        let request = crate::SendMessageRequest {
            codex_skills: vec![],
            take_over: false,
            project_model: None,
            device_id: "owner".into(),
            client_message_id: uuid::Uuid::new_v4().to_string(),
            body: "Please inspect".into(),
            attachment_ids: vec![file_id.clone()],
        };
        let response = crate::send_message_inner(
            State(state.clone()),
            Path(conversation.clone()),
            None,
            Json(crate::SendMessageRequest {
                codex_skills: vec![],
                take_over: false,
                client_message_id: request.client_message_id.clone(),
                attachment_ids: request.attachment_ids.clone(),
                body: request.body.clone(),
                device_id: request.device_id.clone(),

                project_model: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let workspace = projects::media_workspace(&state, &conversation)
            .await
            .unwrap();
        let copy_id = crate::deterministic_uuid(&format!("artifact-source:{file_id}"));
        let copy = crate::attachment_path(&workspace, &copy_id, false)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&copy).unwrap(), source_bytes);
        if diff_note {
            let text = input[0]["text"].as_str().unwrap();
            assert!(text.contains("unified patch, not current file contents"));
            assert!(text.contains(if diff_side == Some("old") {
                "old 2, new —"
            } else {
                "old —, new 2"
            }));
            assert!(!text.contains("Anchor: lines"));
        }
        let saved = state
            .store
            .message_by_device_and_client_message_id("owner", &request.client_message_id)
            .await
            .unwrap()
            .unwrap();
        let files = state
            .store
            .attachments_for_message(&saved.id)
            .await
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, file_id);
        std::fs::write(&source_file, b"changed\n").unwrap();
        assert_eq!(
            selected_inputs(&state, &conversation, &selected, true)
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        let frozen = selected_inputs(&state, &conversation, &selected, false)
            .await
            .unwrap();
        assert!(frozen[0]["text"]
            .as_str()
            .unwrap()
            .contains(copy.to_str().unwrap()));
        assert!(frozen[0]["text"]
            .as_str()
            .unwrap()
            .contains(expected_excerpt));
        assert_eq!(std::fs::read(&copy).unwrap(), source_bytes);
        // An accepted retry remains accepted; new intent must reopen the preview.
        let retry = crate::send_message_inner(
            State(state.clone()),
            Path(conversation.clone()),
            None,
            Json(crate::SendMessageRequest {
                codex_skills: vec![],
                take_over: false,
                client_message_id: request.client_message_id.clone(),
                attachment_ids: request.attachment_ids.clone(),
                body: request.body.clone(),
                device_id: request.device_id.clone(),

                project_model: None,
            }),
        )
        .await;
        assert_eq!(retry.status(), StatusCode::ACCEPTED);
        let fresh = crate::send_message_inner(
            State(state.clone()),
            Path(conversation.clone()),
            None,
            Json(crate::SendMessageRequest {
                codex_skills: vec![],
                take_over: false,
                client_message_id: uuid::Uuid::new_v4().to_string(),
                ..request
            }),
        )
        .await;
        assert_eq!(fresh.status(), StatusCode::CONFLICT);
        assert_eq!(std::fs::read(&source_file).unwrap(), b"changed\n");
        std::fs::write(
            &source_file,
            if diff_note {
                b"first line\nreplacement line\n".as_slice()
            } else {
                b"first line\nsecond line\n".as_slice()
            },
        )
        .unwrap();
        let reused = crate::send_message_inner(
            State(state.clone()),
            Path(conversation.clone()),
            None,
            Json(crate::SendMessageRequest {
                codex_skills: vec![],
                take_over: false,
                project_model: None,
                device_id: "owner".into(),
                client_message_id: uuid::Uuid::new_v4().to_string(),
                body: "Send again".into(),
                attachment_ids: vec![file_id],
            }),
        )
        .await;
        assert_eq!(reused.status(), StatusCode::CONFLICT);
        assert!(String::from_utf8(
            axum::body::to_bytes(reused.into_body(), 8192)
                .await
                .unwrap()
                .to_vec()
        )
        .unwrap()
        .contains("already sent"));

        // Changing the Project's root map blocks new acceptance without stranding
        // an already accepted annotation or silently rebasing its selected rows.
        state
            .store
            .update_project_roots(&project.id, project.roots_revision, &roots[..1], 0, "later")
            .await
            .unwrap();
        assert_eq!(
            selected_inputs(&state, &conversation, &selected, true)
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            selected_inputs(&state, &conversation, &selected, false)
                .await
                .unwrap(),
            frozen
        );

        // Dispatch must not reconstruct a missing accepted copy from mutable
        // source bytes, even if those bytes now happen to match the hash.
        std::fs::remove_file(&copy).unwrap();
        assert_eq!(
            selected_inputs(&state, &conversation, &selected, false)
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        assert!(!copy.exists());
    }
}
