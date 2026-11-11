//! `wonder_show_visual`: an agent Cupola runs shows the reader a visual, the
//! counterpart of T3 Code's `html_render`. The page (HTML) or a wonder-ui v1
//! document is kept as a verified conversation file named by the call's
//! result; `tool_media::normalize` links it to the call exactly as it links
//! a published T3 page, so the phone draws both the same way.
//!
//! The result tells the agent what the phone will show. This is a bounded
//! inventory read here in Rust (tags, styles, embedded pictures), not the
//! phone's converter, so its part counts are estimates; what it reports as
//! left out (scripts aside) the converter never draws.
//!
//! With `preview: true` nothing is posted. On a Mac the signed Cupola.app's
//! `WonderVisualRenderer render-visual` draws the visual with the phone's own
//! views, and the result carries those pictures and the converter's exact
//! part counts; elsewhere it is the checks alone (`exact: false`). The pictures reach the
//! agent as image content and stay in the call's activity details.
use super::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(in crate::thread_tools) const NAME: &str = "wonder_show_visual";
/// A conversation file holds at most 8 MiB; a page keeps headroom under the
/// runtimes' 8–16 MiB message frames once JSON escaping is added.
pub(crate) const HTML_BYTES: usize = 6 * 1024 * 1024;
/// The phone's wonder-ui decoder refuses a larger document.
const COMPONENT_BYTES: usize = 32 * 1024;
/// The call's arguments carry the page; JSON escaping can double its size.
pub(in crate::thread_tools) const ARGUMENT_BYTES: usize = 2 * HTML_BYTES + 64 * 1024;
pub(crate) const COMPONENTS_MIME: &str = "application/vnd.wonder.ui+json";
/// The source of every visual's conversation file starts with this.
pub(crate) const SOURCE_PREFIX: &str = "wonder-visual:";
/// Parts the conversation row shows before Expand (PublishedPageView).
const INLINE_PARTS: usize = 16;
/// The phone's page bounds (HTMLVisualPage.Limits).
const PICTURES: usize = 24;
const PICTURE_BYTES: usize = 4 * 1024 * 1024;
const PICTURE_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const DRAWINGS: usize = 8;
const DRAWING_BYTES: usize = 256 * 1024;
const PAGE_PARTS: usize = 400;
const DISPLAY_ITEMS: usize = 80;
/// A preview draws the phone's width in points at the standard text size and
/// one large accessibility size (SwiftUI's DynamicTypeSize names).
const PREVIEW_WIDTH: u32 = 390;
const PREVIEW_SIZES: [(&str, &str); 2] = [("standard", "large"), ("large text", "accessibility1")];
/// Each preview picture's largest PNG; the helper scales a long visual down.
const PREVIEW_PNG_BYTES: usize = 1024 * 1024;
const PREVIEW_OUTPUT_BYTES: usize = PREVIEW_SIZES.len() * PREVIEW_PNG_BYTES * 4 / 3 + 64 * 1024;
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(30);
/// Previews drawn at once; a page's scripts run in an offscreen web view.
const PREVIEW_RENDERS: usize = 2;

const DESCRIPTION: &str = "Show the reader a visual in this conversation: drawn natively on their phone in their own theme, its opening parts inline and the whole of it behind Expand. Use it when a picture beats prose on a small screen: comparisons, plans, mockups, tables, dashboards, timelines and visual explanations. Pass title and exactly one of components (a wonder-ui v1 document; prefer it for simple data such as tables, stats, charts, steps, checklists and callouts) or html (a self-contained page; prefer it for layout and mockups). In HTML, colour with CSS variables so the reader's theme applies: --background, --foreground, --muted, --muted-foreground, --card, --border, --primary, --accent, --success, --warning, --destructive, --info, --chart-1 to --chart-6 and --radius; literal colours are only approximated by the nearest theme tone. Size with px, %, em or rem, never vh. Embed pictures as data: URLs (PNG, JPEG, GIF or WebP); nothing is fetched. Scripts run once offscreen without network, and only the page they leave is shown. The result lists what will show, what was left out and why, and warnings: fix them and call again with replace set to the returned visualId, and the earlier visual shows as updated. To check how it will look first, pass preview true: nothing is posted, and the result shows it at phone width in standard and large text where the computer can draw it. The visual does not replace your reply; still answer in text.";

pub(in crate::thread_tools) fn spec() -> Value {
    json!({"type":"function","name":NAME,"description":DESCRIPTION,"inputSchema":{
    "type":"object","additionalProperties":false,"required":["title"],"properties":{
        "title":{"type":"string","maxLength":120,"description":"A short name, shown as the visual's heading."},
        "html":{"type":"string","maxLength":HTML_BYTES,"description":"A self-contained HTML page or fragment, at most 6 MB including embedded pictures."},
        "components":{"type":"object","description":"A wonder-ui v1 document: {\"version\":1,\"fallback\":\"the facts in plain text\",\"components\":[{\"id\":\"plan\",\"type\":\"table\",\"title\":\"Plan\",\"columns\":[...],\"rows\":[[...]]}]}. Types: card, table, list, keyValue, chart (bars, or labels with series), progress, diagram, callout, stats, steps, checklist, badges, mockup. At most 24 components, 80 data items and 32 KB."},
        "replace":{"type":"string","description":"The visualId of an earlier visual in this conversation that this one updates."},
        "preview":{"type":"boolean","description":"true to see the visual without posting it: pictures at phone width (390 points) in standard and large text, and exact part counts, where this computer can draw them; elsewhere the checks alone."}
    }}})
}

/// What the phone will show, what it leaves out and why, and what to fix.
#[derive(Default)]
struct Report {
    shows: BTreeMap<&'static str, usize>,
    omitted: BTreeMap<&'static str, (usize, &'static str)>,
    warnings: Vec<String>,
}
impl Report {
    fn show(&mut self, kind: &'static str) {
        *self.shows.entry(kind).or_default() += 1;
    }
    fn omit(&mut self, what: &'static str, why: &'static str) {
        self.omitted.entry(what).or_insert((0, why)).0 += 1;
    }
    fn parts(&self) -> usize {
        self.shows.values().sum()
    }
}

pub(in crate::thread_tools) async fn show(
    state: &AppState,
    caller: &Caller,
    call_id: &str,
    args: Value,
) -> Result<Value, ToolError> {
    only_known(
        &args,
        &["title", "html", "components", "replace", "preview"],
    )?;
    let preview = match args.get("preview") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(preview)) => *preview,
        Some(_) => return Err(ToolError::new("invalid", "preview must be true or false.")),
    };
    let title = crate::tool_media::page_title(Some(&text_arg(&args, "title", 480)?));
    let present = |key| args.get(key).is_some_and(|value| !value.is_null());
    let (bytes, mime, extension, kind, report) = match (present("html"), present("components")) {
        (true, false) => {
            let html = args["html"]
                .as_str()
                .filter(|html| !html.trim().is_empty())
                .ok_or_else(|| ToolError::new("invalid", "html must be a non-empty string."))?;
            if html.len() > HTML_BYTES {
                return Err(ToolError::new(
                    "too_large",
                    "html is larger than 6 MB. Shrink or drop embedded pictures, or split it into two visuals.",
                ));
            }
            let report = inspect_html(html);
            (
                html.as_bytes().to_vec(),
                "text/html",
                "html",
                "html",
                report,
            )
        }
        (false, true) => {
            let report = check_components(&args["components"])?;
            let bytes = serde_json::to_vec(&args["components"]).unwrap_or_default();
            (bytes, COMPONENTS_MIME, "json", "components", report)
        }
        _ => {
            return Err(ToolError::new(
                "invalid",
                "Pass exactly one of html or components.",
            ))
        }
    };
    let conversation = caller.conversation.conversation_id.as_str();
    let replaces = match args.get("replace") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) => {
            let files = state.store.list_conversation_files(conversation).await?;
            files
                .iter()
                .find(|file| &file.id == id && is_visual(file))
                .ok_or_else(|| {
                    ToolError::new(
                        "not_found",
                        "No visual with that visualId in this conversation.",
                    )
                })?;
            Some(id.clone())
        }
        Some(_) => return Err(ToolError::new("invalid", "replace must be a visualId.")),
    };
    if preview {
        let source = if kind == "html" {
            json!({"html": args["html"]})
        } else {
            json!({"components": args["components"]})
        };
        let drawn = match renderer() {
            Some(program) => {
                render(
                    &program,
                    &render_request(&title, kind, source),
                    PREVIEW_TIMEOUT,
                )
                .await
            }
            None => {
                Err("This computer can't draw the phone's view, so these are the checks alone.")
            }
        };
        return Ok(preview_result(&title, kind, report, drawn, replaces));
    }
    let storage = crate::projects::media_workspace(state, conversation)
        .await
        .ok_or_else(|| {
            ToolError::new("unavailable", "Cupola can't keep visuals for this thread.")
        })?;
    let message = caller.message.id.as_str();
    let source_id = format!("{SOURCE_PREFIX}{message}:{call_id}");
    let seed = format!("{SOURCE_PREFIX}{conversation}:{message}:{call_id}");
    let reference = crate::tool_media::store(
        state,
        conversation,
        &storage,
        &seed,
        &source_id,
        &format!("{title}.{extension}"),
        mime,
        &bytes,
    )
    .await
    .map_err(|_| {
        ToolError::new(
            "unavailable",
            "Cupola couldn't save this visual. Try again.",
        )
    })?;
    let mut result = json!({
        "visualId": reference["file"]["id"],
        "title": title,
        "kind": kind,
        "shows": report.shows,
        "inConversation": in_conversation(kind, report.parts(), false),
        "omitted": report.omitted.iter().map(|(what, (count, why))| json!({"what":what,"count":count,"why":why})).collect::<Vec<_>>(),
        "warnings": report.warnings,
    });
    if let Some(replaces) = replaces {
        // The Project's Outputs index links the revision to what it updates.
        if let Some(id) = reference["file"]["id"].as_str() {
            let _ = state
                .store
                .set_conversation_file_replaces(conversation, id, &replaces)
                .await;
        }
        result["replaces"] = json!(replaces);
    }
    Ok(result)
}

fn in_conversation(kind: &str, parts: usize, exact: bool) -> String {
    if kind == "components" || parts <= INLINE_PARTS {
        "The whole visual shows in the conversation; Expand opens it full screen.".to_owned()
    } else {
        let about = if exact { "" } else { "about " };
        format!("The first {INLINE_PARTS} of {about}{parts} parts show in the conversation; Expand opens the rest.")
    }
}

pub(crate) fn is_visual(file: &wonder_store::StoredConversationFile) -> bool {
    file.state == "available"
        && file
            .source_id
            .as_deref()
            .is_some_and(|source| source.starts_with(SOURCE_PREFIX))
}

// ---------------------------------------------------------------------------
// preview
// ---------------------------------------------------------------------------

/// What the helper drew: the converter's exact part counts and one PNG per
/// preview size, in `PREVIEW_SIZES` order.
#[derive(Debug)]
struct Drawn {
    shows: serde_json::Map<String, Value>,
    pictures: Vec<Picture>,
}
#[derive(Debug)]
struct Picture {
    width: u32,
    height: u32,
    png: Vec<u8>,
}

/// The program that draws previews: `WONDER_VISUAL_RENDERER` when set (a
/// development build outside the bundle), otherwise the renderer bundled in
/// the Cupola.app this daemon runs from. It is its own executable: the
/// computer-use helper treats unknown arguments as a control session. Other
/// hosts, and a bundle from before previews, have none.
fn renderer() -> Option<PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            if let Some(path) = std::env::var_os("WONDER_VISUAL_RENDERER") {
                return Some(PathBuf::from(path));
            }
            if !cfg!(target_os = "macos") {
                return None;
            }
            let exe = crate::canonical_path::canonicalize(std::env::current_exe().ok()?).ok()?;
            let contents = exe.parent()?.parent()?;
            (contents.file_name()? == "Contents")
                .then(|| contents.join("Helpers/WonderVisualRenderer"))
        })
        .clone()
        .filter(|path| path.is_file())
}

/// The `render-visual` request: the visual as the tool received it, the
/// width, the text sizes and the PNG bound. The helper applies the phone's
/// default theme and HTMLVisualPage limits.
fn render_request(title: &str, kind: &str, source: Value) -> Value {
    let mut request = json!({
        "version": 1,
        "title": title,
        "kind": kind,
        "widthPoints": PREVIEW_WIDTH,
        "dynamicTypeSizes": PREVIEW_SIZES.iter().map(|(_, size)| *size).collect::<Vec<_>>(),
        "maxPNGBytes": PREVIEW_PNG_BYTES,
    });
    if let (Some(request), Value::Object(source)) = (request.as_object_mut(), source) {
        request.extend(source);
    }
    request
}

/// Runs `program render-visual` with the request on stdin and reads its JSON
/// answer from stdout, bounded in size and time; the child dies with the call.
async fn render(program: &Path, request: &Value, limit: Duration) -> Result<Drawn, &'static str> {
    static RENDERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(PREVIEW_RENDERS);
    const FAILED: &str = "Cupola couldn't draw this preview, so these are the checks alone.";
    let _turn = RENDERS.acquire().await.map_err(|_| FAILED)?;
    let input = serde_json::to_vec(request).map_err(|_| FAILED)?;
    let mut child = tokio::process::Command::new(program)
        .arg("render-visual")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| FAILED)?;
    let (mut stdin, stdout) = (child.stdin.take(), child.stdout.take());
    let run = async {
        let write = async {
            if let Some(stdin) = stdin.as_mut() {
                // A helper that answers without reading everything is fine.
                let _ = stdin.write_all(&input).await;
            }
            drop(stdin.take());
        };
        let read = async {
            let mut output = Vec::new();
            if let Some(stdout) = stdout {
                stdout
                    .take(PREVIEW_OUTPUT_BYTES as u64 + 1)
                    .read_to_end(&mut output)
                    .await
                    .map_err(|_| FAILED)?;
            }
            Ok::<_, &'static str>(output)
        };
        let ((), output) = tokio::join!(write, read);
        let output = output?;
        // An answer past the bound is refused without waiting for the rest.
        if output.len() > PREVIEW_OUTPUT_BYTES {
            return Err(FAILED);
        }
        let status = child.wait().await.map_err(|_| FAILED)?;
        Ok::<_, &'static str>((status, output))
    };
    let (status, output) = tokio::time::timeout(limit, run).await.map_err(|_| {
        "Drawing the preview took longer than 30 seconds, so these are the checks alone."
    })??;
    if !status.success() {
        return Err(FAILED);
    }
    drawn(&output).ok_or(FAILED)
}

/// The helper's answer, checked: part counts are small whole numbers under
/// short names, and each picture is a PNG within the bound.
fn drawn(output: &[u8]) -> Option<Drawn> {
    let answer: Value = serde_json::from_slice(output).ok()?;
    let shows = answer.get("shows")?.as_object()?;
    if shows.len() > 64
        || !shows.iter().all(|(name, count)| {
            (1..=40).contains(&name.len())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ')
                && count.as_u64().is_some_and(|count| count <= 100_000)
        })
    {
        return None;
    }
    let images = answer.get("images")?.as_array()?;
    if images.len() != PREVIEW_SIZES.len() {
        return None;
    }
    let pictures = PREVIEW_SIZES
        .iter()
        .zip(images)
        .map(|((_, size), image)| {
            if image.get("dynamicTypeSize")?.as_str()? != *size {
                return None;
            }
            let encoded = image.get("png")?.as_str()?;
            if encoded.len() > PREVIEW_PNG_BYTES * 4 / 3 + 4 {
                return None;
            }
            let png = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()?;
            let (width, height) = png_size(&png)?;
            (png.len() <= PREVIEW_PNG_BYTES).then_some(Picture { width, height, png })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Drawn {
        shows: shows.clone(),
        pictures,
    })
}

/// A PNG's pixel size from its header, or None when it isn't a PNG.
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
    if png.len() < 24 || png[..8] != SIGNATURE || &png[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(png[20..24].try_into().ok()?);
    (width > 0 && height > 0 && width <= 16_384 && height <= 65_536).then_some((width, height))
}

fn preview_result(
    title: &str,
    kind: &str,
    report: Report,
    drawn: Result<Drawn, &'static str>,
    replaces: Option<String>,
) -> Value {
    let estimate = report.parts();
    let mut result = json!({
        "preview": true,
        "posted": false,
        "title": title,
        "kind": kind,
        "omitted": report.omitted.iter().map(|(what, (count, why))| json!({"what":what,"count":count,"why":why})).collect::<Vec<_>>(),
        "warnings": report.warnings,
        "next": "Nothing was posted. Fix what the pictures and warnings show, then call again without preview to show it.",
    });
    match drawn {
        Ok(drawn) => {
            let parts = drawn.shows.values().filter_map(Value::as_u64).sum::<u64>() as usize;
            result["exact"] = json!(true);
            result["shows"] = Value::Object(drawn.shows);
            result["inConversation"] = json!(in_conversation(kind, parts, true));
            result["previewImage"] = PREVIEW_SIZES
                .iter()
                .zip(drawn.pictures)
                .map(|((text, _), picture)| {
                    json!({
                        "textSize": text,
                        "widthPoints": PREVIEW_WIDTH,
                        "pixels": [picture.width, picture.height],
                        "imageUrl": format!(
                            "data:image/png;base64,{}",
                            base64::engine::general_purpose::STANDARD.encode(&picture.png)
                        ),
                    })
                })
                .collect();
        }
        Err(reason) => {
            result["exact"] = json!(false);
            result["shows"] = json!(report.shows);
            result["inConversation"] = json!(in_conversation(kind, estimate, false));
            result["previewUnavailable"] = json!(reason);
        }
    }
    if let Some(replaces) = replaces {
        result["replaces"] = json!(replaces);
    }
    result
}

/// Moves a preview's pictures out of the result into image content, so the
/// agent sees them as pictures and its text keeps only their sizes.
pub(in crate::thread_tools) fn take_images(result: &mut Value) -> Vec<Value> {
    let Some(images) = result.get_mut("previewImage").and_then(Value::as_array_mut) else {
        return Vec::new();
    };
    images
        .iter_mut()
        .filter_map(|image| image.as_object_mut()?.remove("imageUrl"))
        .map(|url| json!({"type":"inputImage","imageUrl":url}))
        .collect()
}

// ---------------------------------------------------------------------------
// components
// ---------------------------------------------------------------------------

struct Catalog {
    document: jsonschema::Validator,
    /// Each component type's schema branches (`chart` has two).
    kinds: Vec<(String, jsonschema::Validator)>,
}

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut schema: Value = serde_json::from_str(include_str!(
            "../../../../packages/protocol/schemas/wonder-ui-v1.json"
        ))
        .expect("wonder-ui schema");
        let branches = schema["properties"]["components"]["items"]["oneOf"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        // The document is checked here; each component against its own
        // type's branch, so an error names the property that is wrong.
        schema["properties"]["components"]["items"] = json!({"type":"object"});
        let build = |schema: &Value| {
            jsonschema::options()
                .offline()
                .build(schema)
                .expect("wonder-ui schema compiles")
        };
        Catalog {
            document: build(&schema),
            kinds: branches
                .iter()
                .filter_map(|branch| {
                    let kind = branch.pointer("/properties/type/const")?.as_str()?;
                    Some((kind.to_owned(), build(branch)))
                })
                .collect(),
        }
    })
}

fn schema_errors(validator: &jsonschema::Validator, value: &Value, at: &str) -> Vec<String> {
    validator
        .iter_errors(value)
        .take(3)
        .map(|error| {
            let path = error.instance_path().to_string();
            format!("{at}{path}: {error}")
        })
        .collect()
}

fn check_components(document: &Value) -> Result<Report, ToolError> {
    let invalid = |detail: String| {
        ToolError::new(
            "invalid",
            format!("components is not a valid wonder-ui v1 document. {detail}"),
        )
    };
    if !document.is_object() {
        return Err(invalid("Pass a JSON object.".into()));
    }
    if document.to_string().len() > COMPONENT_BYTES {
        return Err(ToolError::new(
            "too_large",
            "components is larger than 32 KB. Split it into two visuals or use html.",
        ));
    }
    let catalog = catalog();
    let errors = schema_errors(&catalog.document, document, "components");
    if !errors.is_empty() {
        return Err(invalid(errors.join("; ")));
    }
    let mut report = Report::default();
    let mut ids = HashSet::new();
    let mut items = 0;
    for (index, component) in document["components"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let at = format!("components/components/{index}");
        let kind = component["type"].as_str().unwrap_or_default();
        let branches: Vec<_> = catalog
            .kinds
            .iter()
            .filter(|(name, _)| name == kind)
            .collect();
        if branches.is_empty() {
            return Err(invalid(format!(
                "{at}: unknown type {kind:?}. Use card, table, list, keyValue, chart, progress, diagram, callout, stats, steps, checklist, badges or mockup."
            )));
        }
        if matches!(kind, "button" | "form") {
            return Err(invalid(format!(
                "{at}: {kind} acts only in your reply's wonder-ui block, not in a visual. Leave it out."
            )));
        }
        if !branches
            .iter()
            .any(|(_, validator)| validator.is_valid(component))
        {
            let errors = branches
                .iter()
                .map(|(_, validator)| schema_errors(validator, component, &at))
                .min_by_key(Vec::len)
                .unwrap_or_default();
            return Err(invalid(errors.join("; ")));
        }
        if !ids.insert(component["id"].as_str().unwrap_or_default().to_owned()) {
            return Err(invalid(format!(
                "{at}: id is used twice; give each component its own."
            )));
        }
        items += display_items(component);
        report.show(match kind {
            "card" => "card",
            "table" => "table",
            "list" => "list",
            "keyValue" => "keyValue",
            "chart" => "chart",
            "progress" => "progress",
            "diagram" => "diagram",
            "callout" => "callout",
            "stats" => "stats",
            "steps" => "steps",
            "checklist" => "checklist",
            "badges" => "badges",
            _ => "mockup",
        });
    }
    if items > DISPLAY_ITEMS {
        return Err(ToolError::new(
            "too_large",
            format!("components has {items} data items (table cells, list entries, bars…); at most {DISPLAY_ITEMS} show. Split it into two visuals."),
        ));
    }
    Ok(report)
}

/// The phone's count of what a component draws (NativeUIComponent.displayItemCount).
fn display_items(component: &Value) -> usize {
    let count = |key: &str| component[key].as_array().map_or(0, Vec::len);
    match component["type"].as_str().unwrap_or_default() {
        "table" => count("columns") * count("rows"),
        "list" | "stats" | "checklist" | "badges" => count("items"),
        "keyValue" => count("entries"),
        "chart" if component.get("bars").is_some() => count("bars"),
        "chart" => count("labels") * count("series"),
        "diagram" | "steps" => count("steps"),
        "mockup" => {
            count("regions")
                + usize::from(component.get("overlay").is_some())
                + usize::from(component.get("badge").is_some())
        }
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// HTML inventory
// ---------------------------------------------------------------------------

/// The value of attribute `name` in one tag. `lower` is the tag lowercased
/// (same byte offsets as `tag`).
fn attribute<'a>(tag: &'a str, lower: &str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(found) = lower[from..].find(name) {
        let at = from + found;
        from = at + name.len();
        let before = lower[..at].chars().last();
        if !before.is_some_and(char::is_whitespace) {
            continue;
        }
        let rest = lower[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let start = lower.len() - rest.trim_start().len();
        let value = &tag[start..];
        return Some(match value.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let body = &value[1..];
                &body[..body.find(quote).unwrap_or(body.len())]
            }
            _ => {
                let end = value
                    .find(|c: char| c.is_whitespace() || c == '>')
                    .unwrap_or(value.len());
                &value[..end]
            }
        });
    }
    None
}

fn inspect_html(html: &str) -> Report {
    let lower = html.to_ascii_lowercase();
    let mut report = Report::default();
    let mut styles = String::new();
    let (mut pictures, mut picture_bytes, mut drawings, mut scripts) = (0, 0, 0, 0);
    let mut cursor = 0;
    // The end of the element whose body is raw text (`</name`), past its `>`.
    let close = |name: &str, from: usize| -> (usize, usize) {
        let marker = format!("</{name}");
        match lower[from..].find(&marker) {
            Some(at) => {
                let end = lower[from + at..]
                    .find('>')
                    .map_or(lower.len(), |gt| from + at + gt + 1);
                (from + at, end)
            }
            None => (lower.len(), lower.len()),
        }
    };
    while let Some(found) = lower[cursor..].find('<') {
        let start = cursor + found;
        if lower[start..].starts_with("<!--") {
            cursor = lower[start..]
                .find("-->")
                .map_or(lower.len(), |end| start + end + 3);
            continue;
        }
        let rest = &lower[start + 1..];
        let closing = rest.starts_with('/');
        let name: String = rest
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        let Some(gt) = lower[start..].find('>') else {
            break;
        };
        let end = start + gt + 1;
        cursor = end;
        if name.is_empty() || closing {
            continue;
        }
        let (tag, tag_lower) = (&html[start..end], &lower[start..end]);
        if let Some(style) = attribute(tag, tag_lower, "style") {
            styles.push_str(style);
            styles.push(';');
        }
        match name.as_str() {
            "script" => {
                let (body_end, after) = close("script", end);
                let kind = attribute(tag, tag_lower, "type")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if attribute(tag, tag_lower, "src").is_some() {
                    report.omit("external script", "nothing is fetched, so it never runs");
                } else if !(kind.contains("json")
                    || kind.contains("template")
                    || kind.contains("text/plain"))
                    && !html[end..body_end].trim().is_empty()
                {
                    scripts += 1;
                }
                cursor = after;
            }
            "style" => {
                let (body_end, after) = close("style", end);
                styles.push_str(&html[end..body_end]);
                styles.push('\n');
                cursor = after;
            }
            "template" | "noscript" => cursor = close(&name, end).1,
            "svg" => {
                let (_, after) = close("svg", end);
                if drawings < DRAWINGS && after - start <= DRAWING_BYTES {
                    drawings += 1;
                    report.show("drawing");
                } else if after - start > DRAWING_BYTES {
                    report.omit("SVG drawing", "larger than 256 KB");
                } else {
                    report.omit("SVG drawing", "past the 8 drawings a visual shows");
                }
                cursor = after;
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => report.show("heading"),
            "p" => report.show("paragraph"),
            "table" => report.show("table"),
            "ul" | "ol" | "dl" => report.show("list"),
            "pre" => report.show("code"),
            "blockquote" | "aside" => report.show("callout"),
            "details" => report.show("details"),
            "hr" => report.show("rule"),
            "img" => {
                let source = attribute(tag, tag_lower, "src").unwrap_or_default();
                let lower_source = source.trim_start().to_ascii_lowercase();
                let raster = ["png", "jpeg", "jpg", "gif", "webp"]
                    .iter()
                    .any(|kind| lower_source.starts_with(&format!("data:image/{kind};base64,")));
                if lower_source.starts_with("data:image/svg") {
                    report.omit("SVG picture in <img>", "put the <svg> in the page itself");
                } else if !lower_source.starts_with("data:") {
                    report.omit(
                        "linked picture",
                        "nothing is fetched; embed it as a data: URL",
                    );
                } else if !raster {
                    report.omit("picture", "only base64 PNG, JPEG, GIF and WebP show");
                } else {
                    let size = source.len().saturating_sub(source.find(',').unwrap_or(0)) * 3 / 4;
                    if size > PICTURE_BYTES {
                        report.omit("oversized picture", "larger than 4 MB");
                    } else if pictures >= PICTURES || picture_bytes + size > PICTURE_TOTAL_BYTES {
                        report.omit(
                            "picture",
                            "past a visual's 24 pictures or 16 MB of pictures",
                        );
                    } else {
                        pictures += 1;
                        picture_bytes += size;
                        report.show("picture");
                    }
                }
            }
            "canvas" => report.omit("canvas", "drawn by script; use SVG or a wonder-ui chart"),
            "video" | "audio" => report.omit("video or audio", "not played inside a visual"),
            "iframe" | "object" | "embed" | "picture" => {
                report.omit("embedded page or object", "has no native form")
            }
            "form" | "button" | "input" | "select" | "textarea" => {
                report.omit("form control", "a visual is read-only")
            }
            "link" if tag_lower.contains("stylesheet") => report.omit(
                "linked style sheet",
                "nothing is fetched; put styles in <style>",
            ),
            _ => {}
        }
    }
    if scripts > 0 {
        report.warnings.push(format!(
            "{scripts} script{} will run once offscreen, without network, and only the page {} leave{} shows. Prefer writing the content into the page.",
            if scripts == 1 { "" } else { "s" },
            if scripts == 1 { "it" } else { "they" },
            if scripts == 1 { "s" } else { "" },
        ));
    }
    style_warnings(&styles, &mut report);
    if report.parts() == 0 {
        report.warnings.push(
            "No headings, paragraphs, tables, lists or pictures were found, so the visual may show nothing. Put the content in elements, not only in script or CSS."
                .into(),
        );
    } else if report.parts() > PAGE_PARTS {
        report.warnings.push(format!(
            "About {} parts; the phone shows the first {PAGE_PARTS}. Shorten the page or split it.",
            report.parts()
        ));
    }
    report
}

/// Literal colours, viewport units and fetched resources in the page's CSS.
fn style_warnings(styles: &str, report: &mut Report) {
    let lower = styles.to_ascii_lowercase();
    if lower.contains("@import")
        || lower.contains("url(http")
        || lower.contains("url('http")
        || lower.contains("url(\"http")
    {
        report.omit("linked font, style or picture in CSS", "nothing is fetched");
    }
    let mut colours = Vec::new();
    let mut viewport = 0;
    for declaration in lower.split([';', '{', '}']) {
        let Some((property, value)) = declaration.split_once(':') else {
            continue;
        };
        let property = property.trim();
        if property.starts_with("--") || value.contains("var(") {
            continue;
        }
        viewport += viewport_units(value);
        if let Some(colour) = literal_colour(value) {
            colours.push(colour);
        }
    }
    if !colours.is_empty() {
        let mut examples: Vec<&str> = colours.iter().map(String::as_str).collect();
        examples.sort_unstable();
        examples.dedup();
        examples.truncate(4);
        report.warnings.push(format!(
            "{} hard-coded colour{} ({}): the phone maps them to the nearest theme tone. Use var(--success), var(--warning), var(--destructive), var(--info), var(--primary), var(--muted-foreground) or var(--chart-1…6) so the reader's theme applies exactly.",
            colours.len(),
            if colours.len() == 1 { "" } else { "s" },
            examples.join(", ")
        ));
    }
    if viewport > 0 {
        report.warnings.push(format!(
            "{viewport} size{} in vh/vw units: the phone has no viewport for them. Use px, %, em or rem.",
            if viewport == 1 { "" } else { "s" }
        ));
    }
}

fn viewport_units(value: &str) -> usize {
    let bytes = value.as_bytes();
    let mut count = 0;
    for unit in ["vh", "vw", "vmin", "vmax"] {
        let mut from = 0;
        while let Some(found) = value[from..].find(unit) {
            let at = from + found;
            from = at + unit.len();
            let number = at > 0 && (bytes[at - 1].is_ascii_digit() || bytes[at - 1] == b'.')
                || (at > 1
                    && matches!(bytes[at - 1], b'd' | b's' | b'l')
                    && bytes[at - 2].is_ascii_digit());
            let ends = bytes.get(from).is_none_or(|b| !b.is_ascii_alphanumeric());
            if number && ends {
                count += 1;
            }
        }
    }
    count
}

/// The first literal colour in a CSS value: `#rgb`, `#rrggbb` (with alpha)
/// or an `rgb()`/`hsl()` function. Neutral keywords don't count.
fn literal_colour(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    for (index, _) in value.match_indices('#') {
        let digits = bytes[index + 1..]
            .iter()
            .take_while(|b| b.is_ascii_hexdigit())
            .count();
        if matches!(digits, 3 | 4 | 6 | 8) {
            return Some(value[index..index + 1 + digits].to_owned());
        }
    }
    for function in ["rgb(", "rgba(", "hsl(", "hsla("] {
        if let Some(at) = value.find(function) {
            let end = value[at..]
                .find(')')
                .map_or(value.len(), |end| at + end + 1);
            return Some(
                value[at..end]
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(""),
            );
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_inventory_counts_parts_and_says_what_is_left_out() {
        let png = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode([137u8, 80, 78, 71, 13, 10, 26, 10])
        );
        let html = format!(
            r#"<!doctype html><style>.ok{{color:var(--success)}} :root{{--brand:#ff0000}} .bad{{color:#ef4444;border:1px solid rgb(37, 99, 235)}} .tall{{height:100vh;width:50%}}</style>
            <h1>Plan</h1><p>One</p><p style="background:#fef3c7">Two</p><!-- <table> -->
            <table><tr><td>a</td></tr></table><ul><li>x</li></ul><svg viewBox="0 0 10 10"><rect/></svg>
            <img src="{png}"><img src="https://example.com/a.png"><img src="data:image/svg+xml;base64,PHN2Zz4=">
            <iframe src="x"></iframe><canvas></canvas><button>Go</button><script src="app.js"></script>
            <script>document.body.append('x')</script><script type="application/json">{{"a":1}}</script>"#
        );
        let report = inspect_html(&html);
        assert_eq!(
            report.shows,
            BTreeMap::from([
                ("drawing", 1),
                ("heading", 1),
                ("list", 1),
                ("paragraph", 2),
                ("picture", 1),
                ("table", 1)
            ])
        );
        let omitted: Vec<_> = report.omitted.keys().copied().collect();
        assert_eq!(
            omitted,
            [
                "SVG picture in <img>",
                "canvas",
                "embedded page or object",
                "external script",
                "form control",
                "linked picture"
            ]
        );
        let warnings = report.warnings.join("\n");
        assert!(
            warnings.contains("1 script will run once offscreen"),
            "{warnings}"
        );
        // The custom property and var() use are fine; three literals are not.
        assert!(
            warnings.contains("3 hard-coded colours (#ef4444, #fef3c7, rgb(37,99,235))"),
            "{warnings}"
        );
        assert!(warnings.contains("1 size in vh/vw units"), "{warnings}");
    }

    // Contract: the phone scenario's two visuals (apps/ios SnapshotTests
    // fixtures, which the phone's host tests decode too) pass these checks.
    #[test]
    fn the_phone_fixtures_pass_and_report_their_parts() {
        let document: Value = serde_json::from_str(include_str!(
            "../../../../apps/ios/SnapshotTests/Fixtures/visual-tool-components.json"
        ))
        .unwrap();
        let report = check_components(&document).unwrap();
        assert_eq!(
            report.shows,
            BTreeMap::from([("callout", 1), ("chart", 1), ("checklist", 1), ("stats", 1)])
        );
        let page = inspect_html(include_str!(
            "../../../../apps/ios/SnapshotTests/Fixtures/visual-tool-page.html"
        ));
        assert_eq!(
            page.shows,
            BTreeMap::from([("heading", 2), ("paragraph", 3), ("table", 1)])
        );
        assert!(page.omitted.is_empty());
        assert_eq!(page.warnings.len(), 1);
        assert!(page.warnings[0].starts_with("6 hard-coded colours"));
    }

    #[test]
    fn pictures_follow_the_phones_budget_and_an_empty_page_is_flagged() {
        let big = format!(
            "<img src=\"data:image/png;base64,{}\">",
            "A".repeat(PICTURE_BYTES / 3 * 4 + 8)
        );
        let report = inspect_html(&big);
        assert_eq!(report.omitted["oversized picture"].0, 1);
        assert!(report.warnings[0].contains("may show nothing"));
        let many = "<img src=\"data:image/gif;base64,R0lGOD\">".repeat(PICTURES + 2);
        let report = inspect_html(&many);
        assert_eq!(report.shows["picture"], PICTURES);
        assert_eq!(report.omitted["picture"].0, 2);
        assert_eq!(viewport_units("calc(100dvh - 2vw) 10px 3.5vmax"), 3);
        assert_eq!(viewport_units("svh-like 10px"), 0);
    }

    #[test]
    fn components_are_checked_against_the_catalog_with_errors_naming_the_property() {
        let good = json!({"version":1,"fallback":"Two routes.","components":[
            {"id":"t","type":"table","title":"Routes","columns":["Route","Time"],"rows":[["River","45"],["Hill","70"]]},
            {"id":"c","type":"chart","title":"Minutes","labels":["River","Hill"],"series":[{"name":"Time","values":[45,70]}]},
            {"id":"s","type":"stats","title":"Totals","items":[{"label":"Routes","value":"2","tone":"success"}]}]});
        let report = check_components(&good).unwrap();
        assert_eq!(
            report.shows,
            BTreeMap::from([("chart", 1), ("stats", 1), ("table", 1)])
        );
        let message = |doc: Value| check_components(&doc).err().unwrap().message;
        let mut missing = good.clone();
        missing["components"][0]
            .as_object_mut()
            .unwrap()
            .remove("rows");
        let text = message(missing);
        assert!(
            text.contains("components/components/0") && text.contains("rows"),
            "{text}"
        );
        let mut twice = good.clone();
        twice["components"][1]["id"] = json!("t");
        assert!(message(twice).contains("id is used twice"));
        let mut button = good.clone();
        button["components"][2] = json!({"id":"b","type":"button","title":"Go","message":"Go"});
        assert!(message(button).contains("acts only in your reply"));
        let mut unknown = good.clone();
        unknown["components"][2]["type"] = json!("map");
        assert!(message(unknown).contains("unknown type"));
        assert!(message(json!({"version":2,"fallback":"x","components":[]})).contains("components"));
        let rows: Vec<Value> = (0..20)
            .map(|i| json!([i.to_string(), "x", "y", "z", "w"]))
            .collect();
        let large = json!({"version":1,"fallback":"x","components":[
            {"id":"a","type":"table","title":"A","columns":["a","b","c","d","e"],"rows":rows}]});
        assert_eq!(check_components(&large).err().unwrap().code, "too_large");
    }

    /// A stand-in `render-visual` helper: keeps its request and prints `answer`.
    #[cfg(unix)]
    fn helper(dir: &Path, answer: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let program = dir.join("helper");
        std::fs::write(dir.join("answer.json"), answer).unwrap();
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\n[ \"$1\" = render-visual ] || exit 2\ncat > '{0}/request.json'\ncat '{0}/answer.json'\n",
                dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    fn png(width: u32, height: u32, extra: usize) -> Vec<u8> {
        let mut png = vec![137u8, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13];
        png.extend(b"IHDR");
        png.extend(width.to_be_bytes());
        png.extend(height.to_be_bytes());
        png.resize(png.len() + extra, 0);
        png
    }

    fn answer(images: &[(&str, Vec<u8>)]) -> String {
        let images: Vec<Value> = images
            .iter()
            .map(|(size, png)| json!({"dynamicTypeSize":size,"png":base64::engine::general_purpose::STANDARD.encode(png)}))
            .collect();
        json!({"shows":{"heading":2,"table":1,"paragraph":19},"images":images}).to_string()
    }

    // Contract: a preview hands the helper the visual at phone width in both
    // text sizes, and the agent gets the helper's exact counts and its two
    // pictures as image content, not text.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_drawn_preview_carries_exact_counts_and_pictures() {
        let dir = tempfile::tempdir().unwrap();
        let program = helper(
            dir.path(),
            &answer(&[
                ("large", png(1170, 2400, 64)),
                ("accessibility1", png(1170, 5200, 64)),
            ]),
        );
        let html = "<h1>Plan</h1><p>One</p>";
        let request = render_request("Plan", "html", json!({"html": html}));
        let drawn = render(&program, &request, PREVIEW_TIMEOUT).await.unwrap();
        let sent: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("request.json")).unwrap())
                .unwrap();
        assert_eq!(sent["html"], html);
        assert_eq!(sent["widthPoints"], 390);
        assert_eq!(sent["dynamicTypeSizes"], json!(["large", "accessibility1"]));
        assert_eq!(sent["maxPNGBytes"], PREVIEW_PNG_BYTES);

        let mut result = preview_result("Plan", "html", inspect_html(html), Ok(drawn), None);
        assert_eq!(result["exact"], true);
        assert_eq!(result["posted"], false);
        assert_eq!(
            result["shows"],
            json!({"heading":2,"table":1,"paragraph":19})
        );
        assert_eq!(
            result["inConversation"],
            "The first 16 of 22 parts show in the conversation; Expand opens the rest."
        );
        assert_eq!(result["previewImage"][1]["textSize"], "large text");
        assert_eq!(result["previewImage"][1]["pixels"], json!([1170, 5200]));
        let images = take_images(&mut result);
        assert_eq!(images.len(), 2);
        assert_eq!(images[0]["type"], "inputImage");
        let url = images[0]["imageUrl"].as_str().unwrap();
        let encoded = url.strip_prefix("data:image/png;base64,").unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            png(1170, 2400, 64)
        );
        assert!(result["previewImage"][0].get("imageUrl").is_none());
        assert!(!result.to_string().contains("base64"));
    }

    // Contract: a helper that never answers is stopped at the time limit
    // (the paused clock reaches it without waiting) and the call goes on.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_preview_that_takes_too_long_is_stopped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let slow = dir.path().join("slow");
        std::fs::write(&slow, "#!/bin/sh\nexec sleep 600\n").unwrap();
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        let request = render_request("Plan", "html", json!({"html":"<p>x</p>"}));
        let reason = render(&slow, &request, PREVIEW_TIMEOUT).await.unwrap_err();
        assert!(reason.contains("longer than 30 seconds"), "{reason}");
    }

    // Contract: a helper that fails, answers out of bounds or sends
    // something other than a PNG yields the checks alone, never its output.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_preview_the_helper_cannot_draw_falls_back_to_the_checks() {
        let request = render_request("Plan", "html", json!({"html":"<p>x</p>"}));
        let good = |size: usize| png(1170, 2400, size);
        let bad = [
            "not json".to_owned(),
            answer(&[("large", good(8))]),
            answer(&[("accessibility1", good(8)), ("large", good(8))]),
            answer(&[
                ("large", b"GIF89a-not-a-png-at-all".to_vec()),
                ("accessibility1", good(8)),
            ]),
            answer(&[
                ("large", good(PREVIEW_PNG_BYTES)),
                ("accessibility1", good(8)),
            ]),
            json!({"shows":{"<script>":1},"images":[]}).to_string(),
            // Valid, but past the answer's bound.
            answer(&[("large", good(8)), ("accessibility1", good(8))])
                + &" ".repeat(PREVIEW_OUTPUT_BYTES),
        ];
        for output in bad {
            let dir = tempfile::tempdir().unwrap();
            let program = helper(dir.path(), &output);
            assert!(
                render(&program, &request, PREVIEW_TIMEOUT).await.is_err(),
                "{output}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let failing = dir.path().join("failing");
        std::fs::write(&failing, "#!/bin/sh\nexit 3\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let reason = render(&failing, &request, PREVIEW_TIMEOUT)
            .await
            .unwrap_err();

        let html = "<h1>Plan</h1><p style=\"color:#ff0000\">One</p>";
        let mut result = preview_result("Plan", "html", inspect_html(html), Err(reason), None);
        assert_eq!(result["exact"], false);
        assert_eq!(result["shows"], json!({"heading":1,"paragraph":1}));
        assert!(result["warnings"][0].as_str().unwrap().contains("#ff0000"));
        assert!(result.get("previewImage").is_none());
        assert!(take_images(&mut result).is_empty());
    }
}
