//! X11 capture and input. A selected window always uses its own XComposite surface.
use super::*;
use libwebrtc::native::yuv_helper;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use x11rb::{
    connection::Connection,
    protocol::{
        composite::{ConnectionExt as _, Redirect},
        shm::ConnectionExt as _,
        xproto::{AtomEnum, ConnectionExt as _, ImageFormat, MapState, Window},
    },
    rust_connection::RustConnection,
};
struct XConnection {
    conn: RustConnection,
    atoms: std::sync::Mutex<std::collections::BTreeMap<String, u32>>,
}
impl std::ops::Deref for XConnection {
    type Target = RustConnection;
    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}
/// Why no X11 display could be opened. Computer view shares an X11 screen, so
/// a Wayland login (a Wayland socket and no `DISPLAY`) gets its own code.
fn display_error(display: Option<&str>, wayland: bool, detail: String) -> String {
    match display {
        None | Some("") if wayland => "wayland_unsupported".into(),
        None | Some("") => "x11_display_required".into(),
        _ => detail,
    }
}

fn x11() -> Result<(XConnection, usize), String> {
    let (conn, screen) = x11rb::connect(None).map_err(|e| {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var("XDG_SESSION_TYPE")
                .is_ok_and(|kind| kind.eq_ignore_ascii_case("wayland"));
        display_error(
            std::env::var("DISPLAY").ok().as_deref(),
            wayland,
            e.to_string(),
        )
    })?;
    Ok((
        XConnection {
            conn,
            atoms: Default::default(),
        },
        screen,
    ))
}

pub(super) fn display_size() -> Result<(u32, u32), String> {
    let (conn, screen) = x11()?;
    let root = &conn.setup().roots[screen];
    Ok((root.width_in_pixels as u32, root.height_in_pixels as u32))
}

fn atom(conn: &XConnection, name: &str) -> u32 {
    let mut atoms = conn.atoms.lock().unwrap();
    if let Some(atom) = atoms.get(name) {
        return *atom;
    }
    let value = conn
        .intern_atom(false, name.as_bytes())
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom)
        .unwrap_or(0);
    if value != 0 {
        atoms.insert(name.to_owned(), value);
    }
    value
}

fn property(conn: &XConnection, window: Window, name: &str) -> Option<(u8, Vec<u8>)> {
    let reply = conn
        .get_property(false, window, atom(conn, name), AtomEnum::ANY, 0, 1 << 16)
        .ok()?
        .reply()
        .ok()?;
    (reply.type_ != 0).then_some((reply.format, reply.value))
}

fn property32(conn: &XConnection, window: Window, name: &str) -> Option<Vec<u32>> {
    let (format, value) = property(conn, window, name)?;
    (format == 32).then(|| {
        value
            .chunks_exact(4)
            .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    })
}

fn printable(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut end = cleaned.len().min(160);
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    cleaned[..end].trim().to_string()
}

/// The Python helper's eligibility rules for a visible application window.
/// WM_CLASS remains the identity; desktop metadata supplies the display name.
fn window_source(conn: &XConnection, window: Window) -> Option<Value> {
    let attrs = conn.get_window_attributes(window).ok()?.reply().ok()?;
    if attrs.map_state != MapState::VIEWABLE || attrs.override_redirect {
        return None;
    }
    let geometry = conn.get_geometry(window).ok()?.reply().ok()?;
    if geometry.width < 80 || geometry.height < 60 {
        return None;
    }
    if property32(conn, window, "WM_STATE").is_some_and(|s| s.first() == Some(&3)) {
        return None;
    }
    let hidden = atom(conn, "_NET_WM_STATE_HIDDEN");
    if property32(conn, window, "_NET_WM_STATE").is_some_and(|s| s.contains(&hidden)) {
        return None;
    }
    if property32(conn, window, "_NET_WM_WINDOW_OPACITY").is_some_and(|s| s.first() == Some(&0)) {
        return None;
    }
    let excluded: Vec<u32> = [
        "DESKTOP",
        "DOCK",
        "MENU",
        "DROPDOWN_MENU",
        "POPUP_MENU",
        "TOOLTIP",
        "NOTIFICATION",
        "COMBO",
        "DND",
        "SPLASH",
        "UTILITY",
    ]
    .iter()
    .map(|kind| atom(conn, &format!("_NET_WM_WINDOW_TYPE_{kind}")))
    .collect();
    if property32(conn, window, "_NET_WM_WINDOW_TYPE")
        .is_some_and(|types| types.iter().any(|t| excluded.contains(t)))
    {
        return None;
    }
    let (_, class) = property(conn, window, "WM_CLASS")?;
    let class = String::from_utf8_lossy(&class);
    let app = class.trim_matches('\0').split('\0').last()?.to_string();
    let id = format!("app:{app}#{window}");
    if app.is_empty()
        || app.len() > 150
        || id.len() > 160
        || !app
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return None;
    }
    let raw =
        property(conn, window, "_NET_WM_NAME").or_else(|| property(conn, window, "WM_NAME"))?;
    let title = printable(&String::from_utf8_lossy(&raw.1));
    if title.is_empty() {
        return None;
    }
    let app_name = application_name(&app);
    Some(
        json!({"id": id, "kind": "window", "appId": app, "appName": app_name,
        "windowTitle": title, "name": title, "width": geometry.width, "height": geometry.height,
        "scale": 1.0}),
    )
}

pub(super) fn list_windows(apps: &[String], all_windows: bool) -> Result<Vec<Value>, String> {
    let (conn, screen) = x11()?;
    let root = conn.setup().roots[screen].root;
    let windows = match property32(&conn, root, "_NET_CLIENT_LIST_STACKING") {
        Some(list) => list,
        None => {
            conn.query_tree(root)
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?
                .children
        }
    };
    let start = windows.len().saturating_sub(512);
    let mut sources = Vec::new();
    for &window in windows[start..].iter().rev() {
        if let Some(mut source) = window_source(&conn, window) {
            if let Some(icon) = property32(&conn, window, "_NET_WM_ICON").and_then(|v| x11_icon(&v))
            {
                source["iconPNG"] = json!(icon);
            }
            let app = source["appId"].as_str().unwrap_or_default();
            if all_windows || apps.iter().any(|a| a == app) {
                sources.push(source);
                if sources.len() == 32 {
                    break;
                }
            }
        }
    }
    Ok(sources)
}

pub(super) fn capture_capability() -> Value {
    match display_size() {
        Ok((w, h)) if w > 0 && h > 0 => json!({"available": true}),
        Err(error) if error == "wayland_unsupported" => {
            json!({"available": false, "reason": "wayland_unsupported"})
        }
        _ => json!({"available": false, "reason": "no_display"}),
    }
}

#[path = "linux_dock.rs"]
mod linux_dock;

/// `--dock`: installed apps (running first) as pinned, other open windows as recent.
pub(super) fn dock() -> Value {
    linux_dock::dock(list_windows(&[], true).map(|windows| {
        windows
            .iter()
            .filter_map(|window| window["appId"].as_str().map(str::to_owned))
            .collect()
    }))
}

/// `--launch-app ID`: starts a desktop file the dock listed.
pub(super) fn launch_app(id: &str) -> Result<(), String> {
    linux_dock::launch(id)
}

/// `--discover-sources`: one JSON answer per request line, until stdin closes.
pub(super) fn discover_sources() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in std::io::BufRead::lines(stdin.lock()) {
        let Ok(line) = line else { break };
        let answer = serde_json::from_str::<Value>(&line)
            .map_err(|e| e.to_string())
            .and_then(|request| {
                let apps: Vec<String> = request["apps"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let all = request["allWindows"].as_bool() == Some(true);
                let capture = capture_capability();
                let available = capture["available"].as_bool() == Some(true);
                let sources = if available && (all || !apps.is_empty()) {
                    list_windows(&apps, all)?
                } else {
                    vec![]
                };
                let mut answer =
                    json!({"available": available, "sources": sources, "capture": capture});
                while answer.to_string().len() > MAX_LINE_BYTES - 512 {
                    answer["sources"].as_array_mut().unwrap().pop();
                }
                Ok(answer)
            })
            .unwrap_or_else(|_| json!({"error": "source_discovery_failed"}));
        let _ = writeln!(out, "{answer}");
        let _ = out.flush();
    }
}

// ------------------------------------------------------------------ input

/// Names every helper accepts but this host has no key for: the batch goes on.
fn key_ignored_here(key: &str) -> bool {
    matches!(
        key.to_lowercase().as_str(),
        "brightnessup" | "brightnessdown"
    )
}

fn keysym(key: &str) -> Result<String, String> {
    let name = key.to_lowercase();
    let mapped = match name.as_str() {
        "return" | "enter" => "Return",
        "tab" => "Tab",
        "space" => "space",
        "delete" | "backspace" => "BackSpace",
        "forwarddelete" => "Delete",
        "escape" | "esc" => "Escape",
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        "home" => "Home",
        "end" => "End",
        "pageup" => "Prior",
        "pagedown" => "Next",
        "capslock" => "Caps_Lock",
        "shift" => "Shift_L",
        "control" | "command" | "ctrl" => "Control_L",
        "option" | "alt" => "Alt_L",
        "super" | "win" => "Super_L",
        "rightshift" => "Shift_R",
        "rightcontrol" | "rightcommand" => "Control_R",
        // AltGr: the third-level chooser, not a second Alt.
        "rightoption" => "ISO_Level3_Shift",
        "insert" => "Insert",
        "printscreen" => "Print",
        "menu" => "Menu",
        "volumeup" => "XF86AudioRaiseVolume",
        "volumedown" => "XF86AudioLowerVolume",
        "mute" => "XF86AudioMute",
        "playpause" => "XF86AudioPlay",
        "next" => "XF86AudioNext",
        "previous" => "XF86AudioPrev",
        "-" => "minus",
        "=" => "equal",
        "[" => "bracketleft",
        "]" => "bracketright",
        "\\" => "backslash",
        ";" => "semicolon",
        "'" => "apostrophe",
        "," => "comma",
        "." => "period",
        "/" => "slash",
        "`" => "grave",
        _ => {
            if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                if (1..=19).contains(&n) {
                    return Ok(format!("F{n}"));
                }
            }
            let mut chars = name.chars();
            return match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_alphanumeric() => Ok(c.to_string()),
                _ => Err("invalid_input".into()),
            };
        }
    };
    Ok(mapped.to_string())
}

/// Wonder's modifier bits (ControlInputTranslator); Command maps to Control.
const MODIFIERS: [(i64, &str); 4] = [
    (1 << 1, "ctrl"),
    (1 << 3, "ctrl"),
    (1 << 2, "alt"),
    (1 << 0, "shift"),
];
const SCROLL_STEP: f64 = 40.0;

fn key_args(
    key: &str,
    phase: &str,
    modifiers: i64,
    held: &BTreeSet<String>,
) -> Result<Vec<String>, String> {
    let native = keysym(key)?;
    let mut combo: Vec<String> = Vec::new();
    for (bit, name) in MODIFIERS {
        let modifier = keysym(name)?;
        if modifiers & bit != 0
            && modifier != native
            && !combo.contains(&modifier)
            && (phase == "up" || !held.contains(&modifier))
        {
            combo.push(modifier);
        }
    }
    combo.push(native);
    let verb = match phase {
        "press" => "key",
        "down" => "keydown",
        "up" => "keyup",
        _ => return Err("invalid_input".into()),
    };
    let mut args = vec![verb.to_string()];
    if phase == "press" && held.is_empty() {
        args.push("--clearmodifiers".into());
    }
    args.push(combo.join("+"));
    Ok(args)
}

async fn xdotool(args: &[&str]) -> Result<(), String> {
    xdotool_command("xdotool", args, None, Duration::from_secs(3)).await
}
async fn checked_xdotool(st: &InputState, args: &[&str]) -> Result<(), String> {
    st.check()?;
    #[cfg(test)]
    let program = st.command.as_deref().unwrap_or("xdotool");
    #[cfg(not(test))]
    let program = "xdotool";
    let limit = st
        .deadline
        .map(|d| d.saturating_duration_since(Instant::now()))
        .unwrap_or(Duration::from_secs(3))
        .min(Duration::from_secs(3));
    xdotool_command(program, args, Some(st.execution.clone()), limit).await
}
async fn xdotool_command(
    program: &str,
    args: &[&str],
    cancellation: Option<tokio::sync::watch::Receiver<u64>>,
    limit: Duration,
) -> Result<(), String> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "input_failed".to_string())?;
    let result = tokio::select! {
        biased;
        _ = async { match cancellation { Some(mut c) => { let _ = c.changed().await; }, None => std::future::pending::<()>().await } } => Err("lease_not_active".into()),
        result = tokio::time::timeout(limit, child.wait()) => match result {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(_) => Err("input_failed".into()), Err(_) => Err("input_timed_out".into()),
        }
    };
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result
}

/// What a finished `xclip -o` means: text, `None` when the clipboard holds no
/// text (still a delivered copy), or why the copy failed.
fn clipboard_copy_result(
    success: bool,
    output: Vec<u8>,
    stderr: &[u8],
) -> Result<Option<Vec<u8>>, &'static str> {
    if success {
        return Ok((!output.is_empty()).then_some(output));
    }
    let message = String::from_utf8_lossy(stderr);
    if message.contains("not available") {
        // "Error: target STRING not available"
        Ok(None)
    } else if message.contains("Can't open display") {
        Err("display_unavailable")
    } else {
        Err("input_failed")
    }
}

async fn clipboard_copy(program: &str, limit: Duration) -> Result<Option<Vec<u8>>, String> {
    let mut child = tokio::process::Command::new(program)
        .args(["-selection", "clipboard", "-o"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => "xclip_required".to_string(),
            _ => "input_failed".to_string(),
        })?;
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let work = async {
        let (mut output, mut message) = (Vec::new(), Vec::new());
        // Stdout first: an oversized copy stops here while xclip is still
        // writing. Its few stderr bytes fit the pipe meanwhile.
        if let Some(stdout) = stdout {
            stdout
                .take(65537)
                .read_to_end(&mut output)
                .await
                .map_err(|_| "input_failed")?;
        }
        if output.len() > 65536 {
            return Err("clipboard_too_large");
        }
        if let Some(stderr) = stderr {
            let _ = stderr.take(4096).read_to_end(&mut message).await;
        }
        let status = child.wait().await.map_err(|_| "input_failed")?;
        clipboard_copy_result(status.success(), output, &message)
    };
    match tokio::time::timeout(limit, work).await {
        Ok(result) => result.map_err(String::from),
        Err(_) => Err("input_timed_out".into()),
    }
}

/// One xdotool command per release: `mouseup` takes a single button, so
/// `mouseup 1 3` releases 1 and fails on "3".
fn release_commands(buttons: &BTreeSet<String>, keys: &BTreeSet<String>) -> Vec<Vec<String>> {
    let mut commands: Vec<Vec<String>> = buttons
        .iter()
        .map(|button| vec!["mouseup".into(), button.clone()])
        .collect();
    if !keys.is_empty() {
        commands.push(
            std::iter::once("keyup".into())
                .chain(keys.iter().cloned())
                .collect(),
        );
    }
    commands
}

async fn clipboard_paste(st: &InputState, text: &str) -> Result<(), String> {
    if text.len() > 65536 {
        return Err("invalid_input".into());
    }
    // xclip keeps the selection until the application reads it once.
    let mut child = tokio::process::Command::new("xclip")
        .args(["-selection", "clipboard", "-i", "-quiet", "-loops", "1"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "clipboard_unavailable".to_string())?;
    let mut cancellation = st.execution.clone();
    let work = async {
        let mut stdin = child.stdin.take().ok_or("clipboard_unavailable")?;
        stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|_| "clipboard_unavailable")?;
        drop(stdin);
        tokio::time::sleep(Duration::from_millis(50)).await;
        checked_xdotool(st, &["key", "--clearmodifiers", "ctrl+v"])
            .await
            .map_err(|_| "clipboard_unavailable")?;
        tokio::select! {
            biased;
            _ = cancellation.changed() => { let _=child.kill().await; let _=child.wait().await; return Err("lease_not_active"); },
            done = child.wait() => { done.map_err(|_| "clipboard_unavailable")?; }
        }
        Ok::<_, &str>(())
    };
    let limit = st
        .deadline
        .map(|d| d.saturating_duration_since(Instant::now()))
        .unwrap_or(Duration::from_secs(3))
        .min(Duration::from_secs(3));
    match tokio::time::timeout(limit, work).await {
        Ok(result) => result.map_err(String::from),
        Err(_) => Err("clipboard_unavailable".into()),
    }
}

impl Helper {
    pub(crate) async fn release_held(&self, st: &mut InputState) {
        let buttons = std::mem::take(&mut st.held_buttons);
        let keys = std::mem::take(&mut st.held_keys);
        // Attempt every release even if one fails.
        for command in release_commands(&buttons, &keys) {
            let args: Vec<&str> = command.iter().map(String::as_str).collect();
            let _ = xdotool(&args).await;
        }
    }

    pub(crate) async fn deliver(
        &self,
        st: &mut InputState,
        action: &Value,
        result: &mut Value,
    ) -> Result<(), String> {
        st.check()?;
        let bad = || "invalid_input".to_string();
        let (w, h) = st
            .source
            .as_ref()
            .map(|s| {
                (
                    s["width"].as_f64().unwrap_or(1280.0),
                    s["height"].as_f64().unwrap_or(720.0),
                )
            })
            .unwrap_or((1280.0, 720.0));
        match action["type"].as_str() {
            Some("pointer") => {
                let (x, y) = (
                    action["x"].as_f64().ok_or_else(bad)?,
                    action["y"].as_f64().ok_or_else(bad)?,
                );
                if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
                    return Err(bad());
                }
                let phase = action["phase"].as_str().unwrap_or("");
                let button = match action["button"].as_str().unwrap_or("left") {
                    "left" => "1",
                    "middle" => "2",
                    "right" => "3",
                    _ => return Err(bad()),
                };
                if !matches!(phase, "down" | "up" | "move") {
                    return Err(bad());
                }
                if phase == "up" && st.held_buttons.contains(button) {
                    let mapped = if let Some(source) =
                        st.source.as_ref().filter(|s| s["kind"] == "window")
                    {
                        let source = source.clone();
                        tokio::task::spawn_blocking(move || {
                            input_frame(&source, Some((x, y)), false, false)
                        })
                        .await
                        .map_err(|_| "input_failed".to_string())
                        .and_then(|r| r)
                    } else {
                        map_point(x, y, 0, 0, w as u32, h as u32).map(Some)
                    };
                    let moved = match mapped {
                        Ok(Some(point)) => {
                            checked_xdotool(
                                st,
                                &["mousemove", &point.0.to_string(), &point.1.to_string()],
                            )
                            .await
                        }
                        Ok(None) => Err("input_failed".into()),
                        Err(e) => Err(e),
                    };
                    let released = xdotool(&["mouseup", button]).await;
                    if released.is_ok() {
                        st.held_buttons.remove(button);
                    }
                    moved?;
                    released?;
                    return Ok(());
                }
                let mut point = map_point(x, y, 0, 0, w as u32, h as u32)?;
                if let Some(source) = st.source.as_ref().filter(|s| s["kind"] == "window") {
                    let source = source.clone();
                    let raise = st.held_buttons.is_empty();
                    let permission = st.permission();
                    point = tokio::task::spawn_blocking(move || {
                        input_frame_checked(&source, Some((x, y)), false, raise, Some(permission))
                    })
                    .await
                    .map_err(|_| "input_failed")??
                    .ok_or("input_failed")?;
                }
                checked_xdotool(
                    st,
                    &["mousemove", &point.0.to_string(), &point.1.to_string()],
                )
                .await?;
                let button = match action["button"].as_str().unwrap_or("left") {
                    "left" => "1",
                    "middle" => "2",
                    "right" => "3",
                    _ => return Err(bad()),
                };
                match action["phase"].as_str() {
                    Some("down") => {
                        st.held_buttons.insert(button.into());
                        checked_xdotool(st, &["mousedown", button]).await?;
                    }
                    Some("up") => {
                        checked_xdotool(st, &["mouseup", button]).await?;
                        st.held_buttons.remove(button);
                    }
                    Some("move") => {}
                    _ => return Err(bad()),
                }
            }
            Some("scroll") => {
                guard(st, None, true).await?;
                for (key, negative, positive) in [("deltaY", "4", "5"), ("deltaX", "6", "7")] {
                    let delta = action[key].as_f64().ok_or_else(bad)?;
                    if !delta.is_finite() || delta.abs() > 4096.0 {
                        return Err(bad());
                    }
                    let mut clicks = (delta.abs() / SCROLL_STEP).round() as i64;
                    if clicks == 0 && delta != 0.0 {
                        clicks = 1;
                    }
                    if clicks > 0 {
                        let repeat = clicks.min(20).to_string();
                        let button = if delta > 0.0 { positive } else { negative };
                        checked_xdotool(st, &["click", "--repeat", &repeat, button]).await?;
                    }
                }
            }
            Some("key") => {
                let phase = action["phase"].as_str().ok_or_else(bad)?;
                if key_ignored_here(action["key"].as_str().ok_or_else(bad)?) {
                    return Ok(());
                }
                if phase != "up" {
                    guard(st, None, false).await?;
                }
                let args = key_args(
                    action["key"].as_str().ok_or_else(bad)?,
                    phase,
                    action["modifiers"].as_i64().unwrap_or(0),
                    &st.held_keys,
                )?;
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                if phase == "down" {
                    st.held_keys
                        .extend(args.last().unwrap().split('+').map(String::from));
                }
                checked_xdotool(st, &refs).await?;
                let keys = args.last().unwrap().split('+').map(String::from);
                match phase {
                    "down" => st.held_keys.extend(keys),
                    "up" => keys.for_each(|k| {
                        st.held_keys.remove(&k);
                    }),
                    _ => {}
                }
            }
            Some("text") => {
                guard(st, None, false).await?;
                let text = action["text"].as_str().unwrap_or_default();
                if text.len() > 4096 {
                    return Err(bad());
                }
                if !text.is_empty() {
                    checked_xdotool(
                        st,
                        &["type", "--clearmodifiers", "--delay", "0", "--", text],
                    )
                    .await?;
                }
            }
            Some("clipboard") => match action["operation"].as_str() {
                Some("copyToPhone") => match {
                    let mut cancelled = st.execution.clone();
                    tokio::select! { biased;
                        _=cancelled.changed()=>return Err("lease_not_active".into()),
                        copied=clipboard_copy("xclip",st.deadline.map(|d|d.saturating_duration_since(Instant::now())).unwrap_or(Duration::from_secs(3)).min(Duration::from_secs(3)))=>copied?,
                    }
                } {
                    Some(text) => result["clipboardText"] = json!(String::from_utf8_lossy(&text)),
                    // Nothing to copy is still delivered input; control stays.
                    None => result["clipboardReason"] = json!("clipboard_no_text"),
                },
                Some("pasteFromPhone") => {
                    guard(st, None, false).await?;
                    clipboard_paste(st, action["text"].as_str().unwrap_or_default()).await?
                }
                _ => return Err(bad()),
            },
            Some("releaseAll") => self.release_held(st).await,
            _ => return Err(bad()),
        }
        Ok(())
    }
}
fn grab_with_retry<T>(
    mut grab: impl FnMut() -> Result<T, String>,
    mut exact_source: impl FnMut() -> bool,
) -> Result<T, String> {
    for attempt in 0..3 {
        match grab() {
            Err(error) if error == "capture_geometry_changed" && attempt < 2 => {
                if !exact_source() {
                    return Err("source_not_found".into());
                }
            }
            result => return result,
        }
    }
    unreachable!()
}

pub(super) fn capture_loop(job: &CaptureJob) -> Result<(), String> {
    let live = &job.live;
    let stop = &job.stop;
    let window = source_window(&job.selected_source);
    let (conn, screen) = x11()?;
    let root = conn.setup().roots[screen].root;
    let (w, h) = {
        let r = &conn.setup().roots[screen];
        (r.width_in_pixels as u32, r.height_in_pixels as u32)
    };
    if let Some(xid) = window {
        // Composite requires a version handshake before its other requests.
        conn.composite_query_version(0, 4)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        conn.composite_redirect_window(xid, Redirect::AUTOMATIC)
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|e| e.to_string())?;
    }
    let mut shared: Option<SharedPixels> = None;
    let mut next = Instant::now();
    let mut redirected = BTreeSet::new();
    while !stop.load(Ordering::SeqCst) {
        let period = Duration::from_secs_f64(1.0 / live.fps.load(Ordering::SeqCst).max(1) as f64);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > period {
            next = now; // fell behind: drop the missed slots rather than burst
        }
        next += period;
        let buffer = grab_with_retry(
            || {
                let (drawable, fw, fh, pixmap) = match window {
                    None => (root, w, h, None),
                    Some(xid) => {
                        let g = conn
                            .get_geometry(xid)
                            .map_err(|e| e.to_string())?
                            .reply()
                            .map_err(|_| "source_not_found".to_string())?;
                        let (fw, fh) = (g.width as u32, g.height as u32);
                        if window_source(&conn, xid).as_ref().map(|s| &s["id"])
                            != Some(&job.selected_source["id"])
                        {
                            return Err("source_not_found".into());
                        }
                        let pixmap = conn.generate_id().map_err(|e| e.to_string())?;
                        conn.composite_name_window_pixmap(xid, pixmap)
                            .map_err(|e| e.to_string())?;
                        (pixmap, fw, fh, Some(pixmap))
                    }
                };
                if fw == 0 || fh == 0 || fw > 8192 || fh > 8192 {
                    return Err("capture_unavailable".into());
                }
                let size = (fw as usize) * (fh as usize) * 4;
                if shared.as_ref().is_none_or(|p| p.size < size) {
                    shared = Some(SharedPixels::new(&conn, size)?);
                }
                let shared = shared.as_ref().unwrap();
                let grabbed = conn
                    .shm_get_image(
                        drawable,
                        0,
                        0,
                        fw as u16,
                        fh as u16,
                        !0,
                        ImageFormat::Z_PIXMAP.into(),
                        shared.segment,
                        0,
                    )
                    .map_err(|e| e.to_string())
                    .and_then(|c| {
                        c.reply().map_err(|e| match e {
                            x11rb::errors::ReplyError::X11Error(ref x)
                                if matches!(
                                    x.error_kind,
                                    x11rb::protocol::ErrorKind::Match
                                        | x11rb::protocol::ErrorKind::Drawable
                                ) =>
                            {
                                "capture_geometry_changed".to_string()
                            }
                            x11rb::errors::ReplyError::X11Error(ref x)
                                if x.error_kind == x11rb::protocol::ErrorKind::Window =>
                            {
                                "source_not_found".to_string()
                            }
                            _ => "capture_failed".to_string(),
                        })
                    });
                if let Some(pixmap) = pixmap {
                    let _ = conn.free_pixmap(pixmap);
                }
                grabbed?;
                let (w, h) = (fw, fh);
                let mut buffer = I420Buffer::new(w, h);
                let (sy, su, sv) = buffer.strides();
                let (y, u, v) = buffer.data_mut();
                let popup_pixels = window.and_then(|window| {
                    composite_popups(&conn, root, window, w, h, shared.pixels(), &mut redirected)
                });
                yuv_helper::argb_to_i420(
                    popup_pixels.as_deref().unwrap_or_else(|| shared.pixels()),
                    w * 4,
                    y,
                    sy,
                    u,
                    su,
                    v,
                    sv,
                    w as i32,
                    h as i32,
                );
                Ok(buffer)
            },
            || {
                window.is_some_and(|xid| {
                    window_source(&conn, xid).as_ref().map(|s| &s["id"])
                        == Some(&job.selected_source["id"])
                })
            },
        )?;
        job.submit(buffer);
    }
    Ok(())
}

pub(super) fn initialize() {}
pub(super) fn control_capability() -> Value {
    let display = std::env::var_os("DISPLAY").is_some();
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    json!({"available": display && which("xdotool"), "reason": if !display && wayland {Some("wayland_input_unavailable")} else if !display {Some("x11_display_required")} else if !which("xdotool") {Some("xdotool_required")} else {None},
        "provider":"xtest-x11-v1", "requiresAccessibility":false})
}
fn source_window(source: &Value) -> Option<u32> {
    if source["kind"] != "window" {
        return None;
    }
    source["id"].as_str()?.rsplit_once('#')?.1.parse().ok()
}
pub(super) fn resolve_source(requested: Option<&str>) -> Result<Value, String> {
    match requested {
        Some(id) if id.starts_with("app:") => {
            let mut source = list_windows(&[], true)?
                .into_iter()
                .find(|s| s["id"].as_str() == Some(id))
                .ok_or("source_not_found")?;
            source["title"] = source["windowTitle"].clone();
            source["contentRect"] =
                json!({"x":0,"y":0,"width":source["width"],"height":source["height"]});
            Ok(source)
        }
        None | Some("display:x11") => {
            let (w, h) = display_size()?;
            Ok(
                json!({"id":"display:x11","kind":"display","title":std::env::var("DISPLAY").unwrap_or_else(|_|"Display".into()),
                "width":w,"height":h,"scale":1.0,"contentRect":{"x":0,"y":0,"width":w,"height":h}}),
            )
        }
        _ => Err("source_not_found".into()),
    }
}
fn transient_for(conn: &XConnection, mut hit: Window, selected: Window) -> bool {
    let mut seen = BTreeSet::new();
    for _ in 0..32 {
        if hit == selected {
            return true;
        }
        if !seen.insert(hit) {
            return false;
        }
        let Some(owner) =
            property32(conn, hit, "WM_TRANSIENT_FOR").and_then(|v| v.first().copied())
        else {
            return false;
        };
        hit = owner;
    }
    false
}
fn input_frame(
    source: &Value,
    point: Option<(f64, f64)>,
    wheel: bool,
    raise: bool,
) -> Result<Option<(i32, i32)>, String> {
    input_frame_checked(source, point, wheel, raise, None)
}
fn input_frame_checked(
    source: &Value,
    point: Option<(f64, f64)>,
    wheel: bool,
    raise: bool,
    permission: Option<InputPermission>,
) -> Result<Option<(i32, i32)>, String> {
    use x11rb::protocol::xproto::{ConfigureWindowAux, InputFocus, StackMode};
    let (conn, screen) = x11()?;
    let root = conn.setup().roots[screen].root;
    let window = source_window(source).ok_or("source_not_found")?;
    let current = window_source(&conn, window)
        .filter(|s| s["id"] == source["id"])
        .ok_or("source_not_found")?;
    let parent = |child| {
        conn.query_tree(child)
            .map_err(|_| "source_not_found")?
            .reply()
            .map(|r| r.parent)
            .map_err(|_| "source_not_found")
    };
    let mut frame = window;
    for depth in 0..64 {
        let p = parent(frame)?;
        if p == root {
            break;
        }
        if p == 0 || depth == 63 {
            return Err("source_not_found".into());
        }
        frame = p;
    }
    let already_focused = conn
        .get_input_focus()
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|f| f.focus == window || transient_for(&conn, f.focus, window));
    if raise && !already_focused {
        if let Some(permission) = &permission {
            permission.check()?;
        }
        conn.configure_window(
            frame,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )
        .map_err(|_| "input_failed")?
        .check()
        .map_err(|_| "input_failed")?;
        if let Some(permission) = &permission {
            permission.check()?;
        }
        conn.set_input_focus(InputFocus::POINTER_ROOT, window, 0u32)
            .map_err(|_| "input_failed")?
            .check()
            .map_err(|_| "input_failed")?;
    }
    let mut focus = conn
        .get_input_focus()
        .map_err(|_| "input_failed")?
        .reply()
        .map_err(|_| "input_failed")?
        .focus;
    for depth in 0..64 {
        if focus == window || transient_for(&conn, focus, window) {
            break;
        }
        if focus <= 1 || focus == root || depth == 63 {
            return Err("window_not_focused".into());
        }
        focus = parent(focus)?;
    }
    let origin = conn
        .translate_coordinates(window, root, 0, 0)
        .map_err(|_| "source_not_found")?
        .reply()
        .map_err(|_| "source_not_found")?;
    let mut child = 0;
    let mapped = if let Some((x, y)) = point {
        let p = map_point(
            x,
            y,
            origin.dst_x as i32,
            origin.dst_y as i32,
            current["width"].as_u64().unwrap_or(0) as u32,
            current["height"].as_u64().unwrap_or(0) as u32,
        )?;
        let rx = i16::try_from(p.0).map_err(|_| "invalid_input")?;
        let ry = i16::try_from(p.1).map_err(|_| "invalid_input")?;
        child = conn
            .translate_coordinates(root, root, rx, ry)
            .map_err(|_| "input_failed")?
            .reply()
            .map_err(|_| "input_failed")?
            .child;
        Some(p)
    } else {
        None
    };
    if wheel {
        child = conn
            .query_pointer(root)
            .map_err(|_| "input_failed")?
            .reply()
            .map_err(|_| "input_failed")?
            .child;
    }
    if (point.is_some() || wheel)
        && child != frame
        && !(transient_for(&conn, child, window) && popup_was_captured(window, child))
    {
        return Err("window_covered".into());
    }
    Ok(mapped)
}

async fn guard(st: &InputState, point: Option<(f64, f64)>, wheel: bool) -> Result<(), String> {
    if let Some(source) = st.source.as_ref().filter(|s| s["kind"] == "window") {
        let source = source.clone();
        let permission = st.permission();
        tokio::task::spawn_blocking(move || {
            input_frame_checked(&source, point, wheel, true, Some(permission))
        })
        .await
        .map_err(|_| "input_failed")??;
    }
    Ok(())
}

/// The server detaches before local memory is unmapped, including all error exits.
struct SharedPixels<'a> {
    conn: &'a XConnection,
    segment: u32,
    address: *mut libc::c_void,
    size: usize,
}
impl<'a> SharedPixels<'a> {
    fn new(conn: &'a XConnection, size: usize) -> Result<Self, String> {
        let segment = conn.generate_id().map_err(|_| "capture_unavailable")?;
        let id = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        if id < 0 {
            return Err("capture_unavailable".into());
        }
        let address = unsafe { libc::shmat(id, std::ptr::null(), 0) };
        if address == (-1isize) as *mut libc::c_void {
            unsafe {
                libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
            }
            return Err("capture_unavailable".into());
        }
        let attached = conn
            .shm_attach(segment, id as u32, false)
            .map_err(|_| "capture_unavailable")
            .and_then(|c| c.check().map_err(|_| "capture_unavailable"));
        unsafe {
            libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
        }
        if attached.is_err() {
            unsafe {
                libc::shmdt(address);
            }
            return Err("capture_unavailable".into());
        }
        Ok(Self {
            conn,
            segment,
            address,
            size,
        })
    }
    fn pixels(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.address.cast(), self.size) }
    }
}
impl Drop for SharedPixels<'_> {
    fn drop(&mut self) {
        if let Ok(c) = self.conn.shm_detach(self.segment) {
            let _ = c.check();
        }
        unsafe {
            libc::shmdt(self.address);
        }
    }
}

fn application_name(app: &str) -> String {
    for id in [app.to_owned(), app.to_lowercase()] {
        let path = std::path::Path::new("/usr/share/applications").join(format!("{id}.desktop"));
        if std::fs::metadata(&path).is_ok_and(|m| m.len() <= 65536) {
            if let Ok(text) = std::fs::read_to_string(path) {
                let mut in_entry = false;
                for line in text.lines().map(str::trim) {
                    if line.starts_with('[') {
                        in_entry = line == "[Desktop Entry]";
                    }
                    if in_entry {
                        if let Some(name) = line.strip_prefix("Name=") {
                            let name = printable(name);
                            if !name.is_empty() {
                                return name;
                            }
                        }
                    }
                }
            }
        }
    }
    printable(&app.rsplit('.').next().unwrap_or(app).replace('-', " "))
}
fn x11_icon(values: &[u32]) -> Option<String> {
    if values.len() > 4096 {
        return None;
    }
    let (mut offset, mut best) = (0usize, None);
    while offset + 2 <= values.len() {
        let (w, h) = (values[offset] as usize, values[offset + 1] as usize);
        let count = w.checked_mul(h)?;
        if w == 0 || h == 0 || w > 256 || h > 256 || offset + 2 + count > values.len() {
            break;
        }
        let score = w.max(h).abs_diff(20);
        if best.is_none_or(|(s, _, _, _)| score < s) {
            best = Some((score, w, h, offset + 2));
        }
        offset += 2 + count;
    }
    let (_, w, h, start) = best?;
    let mut rgba = Vec::with_capacity(1600);
    for y in 0..20 {
        for x in 0..20 {
            let argb = values[start + (y * h / 20) * w + x * w / 20];
            rgba.extend([
                (argb >> 16) as u8,
                (argb >> 8) as u8,
                argb as u8,
                (argb >> 24) as u8,
            ]);
        }
    }
    icon_png(&rgba)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_missing_display_names_wayland_and_otherwise_x11() {
        assert_eq!(display_error(None, true, "x".into()), "wayland_unsupported");
        assert_eq!(
            display_error(Some(""), true, "x".into()),
            "wayland_unsupported"
        );
        assert_eq!(
            display_error(None, false, "x".into()),
            "x11_display_required"
        );
        // A set DISPLAY that fails keeps the connection error.
        assert_eq!(display_error(Some(":0"), true, "refused".into()), "refused");
    }

    #[test]
    fn resize_grabs_retry_only_transient_errors_and_revalidate_the_exact_source() {
        let mut calls = 0;
        let mut validations = 0;
        let frame = grab_with_retry(
            || {
                calls += 1;
                if calls < 3 {
                    Err("capture_geometry_changed".into())
                } else {
                    Ok("resized frame")
                }
            },
            || {
                validations += 1;
                true
            },
        )
        .unwrap();
        assert_eq!((frame, calls, validations), ("resized frame", 3, 2));
        let mut calls = 0;
        let failed = grab_with_retry::<()>(
            || {
                calls += 1;
                Err("capture_geometry_changed".into())
            },
            || true,
        );
        assert_eq!(failed.unwrap_err(), "capture_geometry_changed");
        assert_eq!(calls, 3);
        for code in ["source_not_found", "capture_failed", "capture_unavailable"] {
            let mut calls = 0;
            let failed = grab_with_retry::<()>(
                || {
                    calls += 1;
                    Err(code.into())
                },
                || panic!("terminal errors must not be masked"),
            );
            assert_eq!(failed.unwrap_err(), code);
            assert_eq!(calls, 1);
        }
        let mut calls = 0;
        let replaced = grab_with_retry::<()>(
            || {
                calls += 1;
                Err("capture_geometry_changed".into())
            },
            || false,
        );
        assert_eq!(replaced.unwrap_err(), "source_not_found");
        assert_eq!(calls, 1);
    }

    #[test]
    fn every_held_button_gets_its_own_release() {
        let buttons = BTreeSet::from(["1".to_owned(), "3".to_owned()]);
        let keys = BTreeSet::from(["Control_L".to_owned(), "Shift_L".to_owned()]);
        assert_eq!(
            release_commands(&buttons, &keys),
            [
                vec!["mouseup", "1"],
                vec!["mouseup", "3"],
                vec!["keyup", "Control_L", "Shift_L"]
            ]
        );
        assert!(release_commands(&BTreeSet::new(), &BTreeSet::new()).is_empty());
    }
    /// `xclip -o` stand-ins with the exits and messages measured on Xvfb.
    #[tokio::test]
    async fn clipboard_copy_tells_no_text_from_failures() {
        let folder = std::env::temp_dir().join(format!("wonder-xclip-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let fake = |name: &str, script: &str| {
            use std::os::unix::fs::PermissionsExt;
            let path = folder.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        };
        let limit = Duration::from_secs(10);
        let copy = |program: String| async move { clipboard_copy(&program, limit).await };
        assert_eq!(
            copy(fake("text", "printf 'caf\\303\\251'")).await,
            Ok(Some("café".as_bytes().to_vec()))
        );
        for script in [
            "echo 'Error: target STRING not available' >&2; exit 1",
            "exit 0",
        ] {
            assert_eq!(copy(fake("none", script)).await, Ok(None), "{script}");
        }
        let missing = folder.join("missing").to_string_lossy().into_owned();
        for (program, code) in [
            (missing, "xclip_required"),
            (
                fake(
                    "display",
                    "echo \"Error: Can't open display: :99\" >&2; exit 1",
                ),
                "display_unavailable",
            ),
            (
                fake("large", "head -c 1000000 /dev/zero | tr '\\0' a"),
                "clipboard_too_large",
            ),
            (
                fake("other", "echo 'Error: something else' >&2; exit 2"),
                "input_failed",
            ),
        ] {
            assert_eq!(copy(program).await, Err(code.to_owned()), "{code}");
        }
        // A zero limit expires on the first poll, while xclip is still running.
        let slow = fake("slow", "exec sleep 30");
        assert_eq!(
            clipboard_copy(&slow, Duration::ZERO).await,
            Err("input_timed_out".to_owned())
        );
        let _ = std::fs::remove_dir_all(&folder);
    }
    #[test]
    fn alt_tab_keeps_the_modifier_held_across_cycles() {
        let held = BTreeSet::from(["Alt_L".to_owned()]);
        assert_eq!(key_args("tab", "press", 4, &held).unwrap(), ["key", "Tab"]);
        assert_eq!(key_args("alt", "up", 0, &held).unwrap(), ["keyup", "Alt_L"]);
        assert_eq!(
            key_args("c", "press", 2, &BTreeSet::new()).unwrap(),
            ["key", "--clearmodifiers", "Control_L+c"]
        );
        for (name, expected) in [
            ("super", "Super_L"),
            ("win", "Super_L"),
            ("alt", "Alt_L"),
            ("ctrl", "Control_L"),
            ("command", "Control_L"),
        ] {
            assert_eq!(keysym(name).unwrap(), expected);
        }
    }
    /// The canonical key-name table (crates/wonderd/RPC.md, "Computer key names").
    #[test]
    fn canonical_key_names_map_to_keysyms() {
        for (name, expected) in [
            ("insert", "Insert"),
            ("printscreen", "Print"),
            ("menu", "Menu"),
            ("f13", "F13"),
            ("f19", "F19"),
            ("volumeup", "XF86AudioRaiseVolume"),
            ("volumedown", "XF86AudioLowerVolume"),
            ("mute", "XF86AudioMute"),
            ("playpause", "XF86AudioPlay"),
            ("next", "XF86AudioNext"),
            ("previous", "XF86AudioPrev"),
            ("rightoption", "ISO_Level3_Shift"),
        ] {
            assert_eq!(keysym(name).unwrap(), expected, "{name}");
        }
        assert!(keysym("f20").is_err());
        assert!(key_ignored_here("BrightnessUp") && key_ignored_here("brightnessdown"));
        assert!(!key_ignored_here("mute"));
    }
    #[test]
    fn icons_are_bounded_png_and_reject_truncated_images() {
        use base64::Engine;
        let icon = x11_icon(&[2, 2, 0xffff0000, 0xff00ff00, 0xff0000ff, 0xffffffff]).unwrap();
        assert!(icon.len() <= 3072);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(icon)
            .unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        for invalid in [vec![], vec![256, 256, 1], vec![0, 2], vec![2, 2, 0]] {
            assert!(x11_icon(&invalid).is_none());
        }
    }
    #[test]
    #[ignore = "run on the unit's isolated Xvfb display"]
    fn x11_identity_movement_cover_and_closed_window_contracts() {
        use x11rb::{
            protocol::xproto::{
                ConfigureWindowAux, CreateWindowAux, PropMode, StackMode, WindowClass,
            },
            wrapper::ConnectionExt as _,
            COPY_DEPTH_FROM_PARENT,
        };
        let (conn, screen) = x11().unwrap();
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id().unwrap();
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            20,
            20,
            300,
            200,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().background_pixel(0xff0000),
        )
        .unwrap()
        .check()
        .unwrap();
        conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"fixture\0wonderfixture\0",
        )
        .unwrap()
        .check()
        .unwrap();
        conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"Unit window",
        )
        .unwrap()
        .check()
        .unwrap();
        conn.map_window(window).unwrap().check().unwrap();
        let source = window_source(&conn, window).unwrap();
        assert_eq!(source["id"], format!("app:wonderfixture#{window}"));
        assert_eq!(
            input_frame(&source, Some((0.25, 0.5)), false, true).unwrap(),
            Some((95, 120))
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let helper = runtime.block_on(async { crate::tests::host() });
        runtime.block_on(async {
            let mut input = helper.input.lock().await;
            input.source = Some(source.clone());
            helper
                .deliver(
                    &mut input,
                    &json!({"type":"pointer","phase":"down","x":0.1,"y":0.1}),
                    &mut json!({}),
                )
                .await
                .unwrap();
            helper
                .deliver(
                    &mut input,
                    &json!({"type":"pointer","phase":"up","x":0.9,"y":0.75}),
                    &mut json!({}),
                )
                .await
                .unwrap();
        });
        let pointer = conn.query_pointer(root).unwrap().reply().unwrap();
        assert_eq!(
            (pointer.root_x, pointer.root_y),
            (289, 169),
            "a drag ends at the up event's final point"
        );
        assert!(!pointer
            .mask
            .contains(x11rb::protocol::xproto::KeyButMask::BUTTON1));
        conn.set_input_focus(
            x11rb::protocol::xproto::InputFocus::POINTER_ROOT,
            root,
            0u32,
        )
        .unwrap()
        .check()
        .unwrap();
        let (execution, receiver) = tokio::sync::watch::channel(0);
        let permission = InputPermission {
            execution: receiver,
            generation: 0,
            deadline: None,
        };
        execution.send_replace(1);
        assert_eq!(
            input_frame_checked(&source, None, false, true, Some(permission)).unwrap_err(),
            "lease_not_active"
        );
        assert_eq!(conn.get_input_focus().unwrap().reply().unwrap().focus, root);
        input_frame(&source, None, false, true).unwrap();
        let cover = conn.generate_id().unwrap();
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            cover,
            root,
            0,
            0,
            500,
            400,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().override_redirect(1),
        )
        .unwrap()
        .check()
        .unwrap();
        conn.map_window(cover).unwrap().check().unwrap();
        conn.configure_window(
            cover,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )
        .unwrap()
        .check()
        .unwrap();
        assert_eq!(
            input_frame(&source, Some((0.25, 0.5)), false, false).unwrap_err(),
            "window_covered"
        );
        conn.change_property32(
            PropMode::REPLACE,
            cover,
            AtomEnum::WM_TRANSIENT_FOR,
            AtomEnum::WINDOW,
            &[window],
        )
        .unwrap()
        .check()
        .unwrap();
        // Ownership alone is insufficient: there are no popup pixels yet.
        assert_eq!(
            input_frame(&source, Some((0.25, 0.5)), false, false).unwrap_err(),
            "window_covered"
        );
        conn.composite_query_version(0, 4).unwrap().reply().unwrap();
        let base = vec![255u8; 300 * 200 * 4];
        let composed =
            composite_popups(&conn, root, window, 300, 200, &base, &mut BTreeSet::new()).unwrap();
        assert_ne!(
            composed, base,
            "the related popup must actually enter the captured surface"
        );
        assert_eq!(
            input_frame(&source, Some((0.25, 0.5)), false, false).unwrap(),
            Some((95, 120))
        );
        conn.destroy_window(cover).unwrap().check().unwrap();
        conn.configure_window(window, &ConfigureWindowAux::new().x(420).y(310))
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(
            input_frame(&source, Some((0.25, 0.5)), false, true).unwrap(),
            Some((495, 410))
        );
        runtime.block_on(async {
            let mut input = helper.input.lock().await;
            helper
                .deliver(
                    &mut input,
                    &json!({"type":"pointer","phase":"down","x":0.25,"y":0.5}),
                    &mut json!({}),
                )
                .await
                .unwrap();
        });
        conn.unmap_window(window).unwrap().check().unwrap();
        assert!(window_source(&conn, window).is_none());
        assert!(resolve_source(source["id"].as_str()).is_err());
        conn.destroy_window(window).unwrap().check().unwrap();
        runtime.block_on(async {
            let mut input = helper.input.lock().await;
            assert_eq!(
                helper
                    .deliver(
                        &mut input,
                        &json!({"type":"pointer","phase":"up","x":0.9,"y":0.75}),
                        &mut json!({})
                    )
                    .await
                    .unwrap_err(),
                "source_not_found"
            );
            assert!(input.held_buttons.is_empty());
        });
        assert!(!conn
            .query_pointer(root)
            .unwrap()
            .reply()
            .unwrap()
            .mask
            .contains(x11rb::protocol::xproto::KeyButMask::BUTTON1));
    }
}

// Own/transient surfaces are composited into the selected window's bounds.
// A same-app window without this relationship never supplies pixels or input.
type PopupCapture = Option<(Window, Instant, BTreeSet<Window>)>;
static POPUPS: std::sync::Mutex<PopupCapture> = std::sync::Mutex::new(None);
fn popup_was_captured(selected: Window, popup: Window) -> bool {
    POPUPS
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|(source, at, windows)| {
            *source == selected
                && at.elapsed() < Duration::from_millis(500)
                && windows.contains(&popup)
        })
}
fn composite_popups(
    conn: &XConnection,
    root: Window,
    selected: Window,
    width: u32,
    height: u32,
    pixels: &[u8],
    redirected: &mut BTreeSet<Window>,
) -> Option<Vec<u8>> {
    let origin = conn
        .translate_coordinates(selected, root, 0, 0)
        .ok()?
        .reply()
        .ok()?;
    let children = conn.query_tree(root).ok()?.reply().ok()?.children;
    redirected.retain(|window| children.contains(window));
    let mut output = None;
    let mut captured = BTreeSet::new();
    // Popup windows are normally at the top. Bound per-frame property reads.
    let candidates: Vec<_> = children.into_iter().rev().take(32).collect();
    for popup in candidates.into_iter().rev() {
        if popup == selected || !transient_for(conn, popup, selected) {
            continue;
        }
        let attrs = conn.get_window_attributes(popup).ok()?.reply().ok()?;
        if attrs.map_state != MapState::VIEWABLE {
            continue;
        }
        let g = conn.get_geometry(popup).ok()?.reply().ok()?;
        let p = conn
            .translate_coordinates(popup, selected, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        let (x, y) = (i32::from(p.dst_x).max(0), i32::from(p.dst_y).max(0));
        let (right, bottom) = (
            (i32::from(p.dst_x) + g.width as i32).min(width as i32),
            (i32::from(p.dst_y) + g.height as i32).min(height as i32),
        );
        if right <= x || bottom <= y {
            continue;
        }
        if !redirected.contains(&popup) {
            if conn
                .composite_redirect_window(popup, Redirect::AUTOMATIC)
                .ok()
                .and_then(|c| c.check().ok())
                .is_none()
            {
                continue;
            }
            redirected.insert(popup);
        }
        let Ok(pixmap) = conn.generate_id() else {
            continue;
        };
        if conn
            .composite_name_window_pixmap(popup, pixmap)
            .ok()
            .and_then(|c| c.check().ok())
            .is_none()
        {
            continue;
        }
        let image = conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                pixmap,
                (x - i32::from(p.dst_x)) as i16,
                (y - i32::from(p.dst_y)) as i16,
                (right - x) as u16,
                (bottom - y) as u16,
                !0,
            )
            .ok()
            .and_then(|c| c.reply().ok());
        let _ = conn.free_pixmap(pixmap);
        let Some(image) = image else {
            continue;
        };
        let (w, h) = ((right - x) as usize, (bottom - y) as usize);
        if image.data.len() != w * h * 4 {
            continue;
        }
        let target = output.get_or_insert_with(|| pixels.to_vec());
        for row in 0..h {
            let offset = ((y as usize + row) * width as usize + x as usize) * 4;
            target[offset..offset + w * 4]
                .copy_from_slice(&image.data[row * w * 4..(row + 1) * w * 4]);
        }
        captured.insert(popup);
    }
    let _ = origin; // Translation above also verifies that the exact source exists.
    *POPUPS.lock().unwrap() = Some((selected, Instant::now(), captured));
    output
}

// AT-SPI is optional. Read only focused/editable state, never field contents.
// One dedicated poller owns the library; missing accessibility means no
// automatic keyboard rather than guessing from an application's name.
type Obj = *mut libc::c_void;
struct Atspi {
    _library: Obj,
    desktop: unsafe extern "C" fn(i32) -> Obj,
    count: unsafe extern "C" fn(Obj, *mut Obj) -> i32,
    child: unsafe extern "C" fn(Obj, i32, *mut Obj) -> Obj,
    pid: unsafe extern "C" fn(Obj, *mut Obj) -> u32,
    states: unsafe extern "C" fn(Obj) -> Obj,
    clear: unsafe extern "C" fn(Obj),
    contains: unsafe extern "C" fn(Obj, i32) -> i32,
    unref: unsafe extern "C" fn(Obj),
}
impl Atspi {
    unsafe fn load() -> Option<Self> {
        let library = libc::dlopen(c"libatspi.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if library.is_null() {
            return None;
        }
        macro_rules! symbol {
            ($name:literal,$ty:ty) => {{
                let pointer = libc::dlsym(library, concat!($name, "\0").as_ptr().cast());
                if pointer.is_null() {
                    libc::dlclose(library);
                    return None;
                }
                std::mem::transmute::<Obj, $ty>(pointer)
            }};
        }
        let init = symbol!("atspi_init", unsafe extern "C" fn() -> i32);
        let timeout = symbol!("atspi_set_timeout", unsafe extern "C" fn(i32, i32));
        let api = Self {
            _library: library,
            desktop: symbol!("atspi_get_desktop", unsafe extern "C" fn(i32) -> Obj),
            count: symbol!(
                "atspi_accessible_get_child_count",
                unsafe extern "C" fn(Obj, *mut Obj) -> i32
            ),
            child: symbol!(
                "atspi_accessible_get_child_at_index",
                unsafe extern "C" fn(Obj, i32, *mut Obj) -> Obj
            ),
            pid: symbol!(
                "atspi_accessible_get_process_id",
                unsafe extern "C" fn(Obj, *mut Obj) -> u32
            ),
            clear: symbol!("atspi_accessible_clear_cache", unsafe extern "C" fn(Obj)),
            states: symbol!(
                "atspi_accessible_get_state_set",
                unsafe extern "C" fn(Obj) -> Obj
            ),
            contains: symbol!(
                "atspi_state_set_contains",
                unsafe extern "C" fn(Obj, i32) -> i32
            ),
            unref: symbol!("g_object_unref", unsafe extern "C" fn(Obj)),
        };
        timeout(100, 100);
        if init() != 0 {
            return None;
        }
        Some(api)
    }
    unsafe fn focused(&self, pid: u32) -> bool {
        let desktop = (self.desktop)(0);
        if desktop.is_null() {
            return false;
        }
        let deadline = Instant::now() + Duration::from_millis(150);
        let mut remaining = 256usize;
        let mut found = false;
        for i in 0..(self.count)(desktop, std::ptr::null_mut()).clamp(0, 64) {
            if Instant::now() >= deadline {
                break;
            }
            let app = (self.child)(desktop, i, std::ptr::null_mut());
            if app.is_null() {
                continue;
            }
            if (self.pid)(app, std::ptr::null_mut()) == pid {
                found = self.visit(app, 0, &mut remaining, deadline);
            }
            (self.unref)(app);
            if found {
                break;
            }
        }
        (self.unref)(desktop);
        found
    }
    unsafe fn visit(
        &self,
        node: Obj,
        depth: u32,
        remaining: &mut usize,
        deadline: Instant,
    ) -> bool {
        if *remaining == 0 || depth > 12 || Instant::now() >= deadline {
            return false;
        }
        *remaining -= 1;
        (self.clear)(node);
        let states = (self.states)(node);
        if !states.is_null() {
            let focused = (self.contains)(states, 12) != 0 && (self.contains)(states, 7) != 0;
            (self.unref)(states);
            if focused {
                return true;
            }
        }
        for i in 0..(self.count)(node, std::ptr::null_mut()).clamp(0, 64) {
            if *remaining == 0 || Instant::now() >= deadline {
                break;
            }
            let child = (self.child)(node, i, std::ptr::null_mut());
            if child.is_null() {
                continue;
            }
            let found = self.visit(child, depth + 1, remaining, deadline);
            (self.unref)(child);
            if found {
                return true;
            }
        }
        false
    }
}
thread_local! { static ATSPI: Option<Atspi> = unsafe { Atspi::load() }; }
pub(super) fn text_input_focused(source: &Value) -> bool {
    let Ok((conn, screen)) = x11() else {
        return false;
    };
    let root = conn.setup().roots[screen].root;
    let window = if let Some(window) = source_window(source) {
        if input_frame(source, None, false, false).is_err() {
            return false;
        }
        window
    } else {
        let Some(window) =
            property32(&conn, root, "_NET_ACTIVE_WINDOW").and_then(|v| v.first().copied())
        else {
            return false;
        };
        window
    };
    let Some(pid) = property32(&conn, window, "_NET_WM_PID").and_then(|v| v.first().copied())
    else {
        return false;
    };
    ATSPI.with(|api| api.as_ref().is_some_and(|api| unsafe { api.focused(pid) }))
}
