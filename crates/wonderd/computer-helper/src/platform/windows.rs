//! Windows capture and guarded SendInput. Runs in the interactive logon session.
use super::*;
#[path = "win.rs"]
mod win;
use libwebrtc::native::yuv_helper;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor,
    settings::*,
    window::Window,
};
#[derive(Debug)]
struct HelperError(&'static str);
impl From<HelperError> for String {
    fn from(e: HelperError) -> Self {
        e.0.into()
    }
}
#[derive(Clone, Debug)]
pub struct Source {
    pub id: String,
    pub kind: &'static str,
    pub title: String,
    pub app_id: Option<String>,
    pub hwnd: Option<isize>,
    pub width: u32,
    pub height: u32,
    pub origin: (i32, i32),
    pub monitor: usize,
}

pub(super) fn input_owner() {
    win::input_owner();
}
pub(super) fn initialize() {
    win::dpi_aware();
    win::connect_input_owner();
}
pub(super) fn capture_capability() -> Value {
    crate::secure_desktop_capability(win::secure_desktop_active())
}
pub(super) fn control_capability() -> Value {
    json!({"available":win::connect_input_owner() && !win::secure_desktop_active(),"provider":"sendinput-win32-v1","requiresAccessibility":false})
}
/// Shows a panel on its own thread and message loop. A question (several
/// buttons) comes to the front; the notice never takes focus.
pub(super) fn show_panel(text: &'static str, buttons: &'static [&'static str]) -> Option<Panel> {
    let ask = buttons.len() > 1;
    let (answer, choice) = tokio::sync::oneshot::channel();
    let (ready, shown) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("computer-panel".into())
        .spawn(move || win::run_panel(text, buttons, ask, answer, ready))
        .ok()?;
    // Creating a window takes milliseconds; a panel that is not up by then
    // is closed by its own thread, and control is refused.
    let window = shown.recv_timeout(Duration::from_secs(2)).ok()??;
    Some(Panel {
        choice,
        window: Box::new(PanelWindow(window)),
    })
}
struct PanelWindow(isize);
impl Drop for PanelWindow {
    fn drop(&mut self) {
        win::close_panel(self.0);
    }
}
pub(super) fn display_size() -> Result<(u32, u32), String> {
    let s = win::display_source(1)?;
    Ok((s.width, s.height))
}
fn source(id: Option<&str>) -> Result<Source, String> {
    match id {
        None => Ok(win::display_source(1)?),
        Some(id) if id.starts_with("display:") => Ok(win::display_source(
            id[8..].parse().map_err(|_| "source_not_found")?,
        )?),
        Some(id) if id.starts_with("app:") => {
            let (app, handle) = id[4..].rsplit_once('#').ok_or("source_not_found")?;
            let h = handle
                .parse::<isize>()
                .ok()
                .filter(|h| *h > 0 && *h <= 0xFFFF_FFFF)
                .ok_or("source_not_found")?;
            Ok(win::selected_window(id, app, h)?)
        }
        _ => Err("source_not_found".into()),
    }
}
pub(super) fn resolve_source(id: Option<&str>) -> Result<Value, String> {
    let s = source(id)?;
    Ok(
        json!({"id":s.id,"kind":s.kind,"title":s.title,"appId":s.app_id,"width":s.width,"height":s.height,"scale":1.0,
        "contentRect":{"x":0,"y":0,"width":s.width,"height":s.height}}),
    )
}
#[path = "windows_dock.rs"]
mod windows_dock;

/// The taskbar's pinned shortcuts that name a local program, by file name.
/// Store (UWP) pins carry no program path and are skipped.
fn dock_pins() -> Vec<windows_dock::Pin> {
    let Some(folder) = std::env::var_os("APPDATA").map(|appdata| {
        std::path::PathBuf::from(appdata)
            .join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar")
    }) else {
        return Vec::new();
    };
    dock_pins_at(&folder)
}

fn dock_pins_at(folder: &std::path::Path) -> Vec<windows_dock::Pin> {
    let mut files: Vec<_> = std::fs::read_dir(folder)
        .map(|listing| listing.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    files.sort();
    files
        .into_iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"))
        })
        .take(64)
        .filter_map(|path| {
            let meta = std::fs::metadata(&path).ok()?;
            if !meta.is_file() || meta.len() > 65536 {
                return None;
            }
            let program = win::shortcut_program(&path)?;
            let name = path.file_stem()?.to_string_lossy().into_owned();
            Some(windows_dock::Pin {
                id: windows_dock::pin_id(&name),
                name,
                program,
                shortcut: path.to_str()?.to_owned(),
            })
        })
        .collect()
}

fn dock_running() -> Vec<windows_dock::Running> {
    win::list_dock_windows()
        .into_iter()
        .filter_map(|window| {
            Some(windows_dock::Running {
                exe: window["appId"].as_str()?.to_owned(),
                source_id: window["id"].as_str()?.to_owned(),
                icon: window["iconPNG"].as_str().map(str::to_owned),
            })
        })
        .collect()
}

/// One-shot data lookup; do not connect to or start the input owner.
pub(super) fn dock() -> Value {
    win::dpi_aware();
    if win::secure_desktop_active() {
        return json!({"available":false,"pinned":[],"recent":[]});
    }
    windows_dock::build(&dock_pins(), &dock_running(), win::file_icon)
}

/// The daemon checks the control lease before invoking this one-shot helper.
/// IDs are resolved afresh; neither paths nor commands are accepted from it.
pub(super) fn launch_app(id: &str) -> Result<(), String> {
    win::dpi_aware();
    if win::secure_desktop_active() {
        return Err("input_desktop_unavailable".into());
    }
    let pins = dock_pins();
    let running = dock_running();
    match windows_dock::launch(id, &pins, &running).ok_or("app_not_found")? {
        windows_dock::Launch::Shortcut(path) => {
            if win::open_file(path) {
                Ok(())
            } else {
                Err("launch_failed".into())
            }
        }
        windows_dock::Launch::Window(id) => {
            let window = source(Some(id))?;
            if win::focus_app_window(&window) {
                Ok(())
            } else {
                Err("window_not_focused".into())
            }
        }
    }
}

pub(super) fn discover_sources() {
    initialize();
    for line in std::io::BufRead::lines(std::io::stdin().lock()) {
        let Ok(line) = line else { break };
        if line.len() > MAX_LINE_BYTES {
            break;
        }
        let answer = (|| -> Result<Value, String> {
            let request: Value =
                serde_json::from_str(&line).map_err(|_| "source_discovery_failed")?;
            let apps: Vec<_> = request["apps"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_lowercase))
                        .collect()
                })
                .unwrap_or_default();
            let all = request["allWindows"] == true;
            let capability = capture_capability();
            let mut answer = json!({"available":capability["available"],"capture":capability,
                "sources":if all||!apps.is_empty(){win::list_windows(&apps,all)}else{vec![]}});
            while answer.to_string().len() > MAX_LINE_BYTES - 512 {
                answer["sources"].as_array_mut().unwrap().pop();
            }
            Ok(answer)
        })()
        .unwrap_or_else(|_| json!({"error":"source_discovery_failed"}));
        let mut out = std::io::stdout().lock();
        if writeln!(out, "{answer}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}
fn foreground() -> isize {
    unsafe { win::root(GetForegroundWindow()) }
}
fn focus(s: &Source, activate: bool, permission: &InputState) -> Result<(), String> {
    if win::secure_desktop_active() {
        return Err("input_desktop_unavailable".into());
    }
    let Some(h) = s.hwnd else { return Ok(()) };
    if win::window_exe(h) != s.app_id {
        return Err("source_not_found".into());
    }
    let owns_focus = || {
        foreground() == h
            || unsafe { GetAncestor(win::hwnd(foreground()), GA_ROOTOWNER).0 as isize == h }
    };
    if !owns_focus() && activate {
        unsafe {
            if IsIconic(win::hwnd(h)).as_bool() {
                permission.check()?;
                let _ = ShowWindow(win::hwnd(h), SW_RESTORE);
            }
            permission.check()?;
            let _ = BringWindowToTop(win::hwnd(h));
            permission.check()?;
            let _ = SetForegroundWindow(win::hwnd(h));
            if foreground() != h {
                let current = GetForegroundWindow();
                let ours = GetCurrentThreadId();
                let theirs = GetWindowThreadProcessId(current, None);
                permission.check()?;
                let attached = theirs != 0
                    && theirs != ours
                    && AttachThreadInput(ours, theirs, true).as_bool();
                let allowed = permission.check();
                if allowed.is_ok() {
                    let _ = BringWindowToTop(win::hwnd(h));
                    if permission.check().is_ok() {
                        let _ = SetForegroundWindow(win::hwnd(h));
                    }
                }
                if attached {
                    let _ = AttachThreadInput(ours, theirs, false);
                }
                allowed?;
                permission.check()?;
            }
        }
    }
    if !owns_focus() {
        return Err("window_not_focused".into());
    }
    Ok(())
}
fn under(s: &Source, p: (i32, i32)) -> Result<(), String> {
    for attempt in 0..6 {
        if win::require_under(s, p).is_ok() {
            return Ok(());
        }
        if attempt < 5 {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    Err("window_covered".into())
}
/// Marks a Control this helper pressed to pair with a held AltGr (Windows treats AltGr as Ctrl+Alt).
const ALTGR_CTRL: &str = "altgr-ctrl";
/// Names every helper accepts but this host has no key for: the batch goes on.
fn key_ignored_here(key: &str) -> bool {
    matches!(
        key.to_lowercase().as_str(),
        "brightnessup" | "brightnessdown"
    )
}
fn virtual_key(key: &str) -> Result<u16, String> {
    let name = key.to_lowercase();
    let vk = match name.as_str() {
        "return" | "enter" => 0x0d,
        "tab" => 9,
        "space" => 0x20,
        "delete" | "backspace" => 8,
        "forwarddelete" => 0x2e,
        "escape" | "esc" => 0x1b,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "capslock" => 0x14,
        "shift" => 0xa0,
        "control" | "ctrl" | "command" => 0xa2,
        "alt" | "option" => 0xa4,
        "win" | "super" => 0x5b,
        "rightshift" => 0xa1,
        "rightcontrol" | "rightcommand" => 0xa3,
        // AltGr: key() adds the Control that Windows pairs with Right Alt.
        "rightoption" => 0xa5,
        "insert" => 0x2d,
        "printscreen" => 0x2c,
        "menu" => 0x5d,
        "volumeup" => 0xaf,
        "volumedown" => 0xae,
        "mute" => 0xad,
        "playpause" => 0xb3,
        "next" => 0xb0,
        "previous" => 0xb1,
        "-" => 0xbd,
        "=" => 0xbb,
        "[" => 0xdb,
        "]" => 0xdd,
        "\\" => 0xdc,
        ";" => 0xba,
        "'" => 0xde,
        "," => 0xbc,
        "." => 0xbe,
        "/" => 0xbf,
        "`" => 0xc0,
        _ => {
            if let Some(n) = name
                .strip_prefix('f')
                .and_then(|n| n.parse::<u16>().ok())
                .filter(|n| (1..=19).contains(n))
            {
                return Ok(0x6f + n);
            }
            if name.len() == 1 && name.as_bytes()[0].is_ascii_alphanumeric() {
                return Ok(name.as_bytes()[0].to_ascii_uppercase() as u16);
            }
            return Err("invalid_input".into());
        }
    };
    Ok(vk)
}
fn key(vk: u16, up: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    if [
        0x25, 0x26, 0x27, 0x28, 0x24, 0x23, 0x21, 0x22, 0x2e, 0x2d, 0x2c, 0x5d, 0x5b, 0xa3, 0xa5,
        0xad, 0xae, 0xaf, 0xb0, 0xb1, 0xb3,
    ]
    .contains(&vk)
    {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}
/// Encode a key action and retain ownership of the Control paired with AltGr.
/// Kept separate from focus/SendInput so the protocol's held-key contract can
/// be checked without sending system keys to the desktop.
fn key_inputs(
    vk: u16,
    phase: &str,
    mask: i64,
    held: &mut BTreeSet<String>,
) -> Result<Vec<INPUT>, String> {
    if !["press", "down", "up"].contains(&phase) {
        return Err("invalid_input".into());
    }
    if phase == "up" {
        let mut items = vec![key(vk, true)];
        if held.remove(&vk.to_string()) {
            for (bit, m) in [(2, 0xa2), (8, 0xa2), (4, 0xa4), (1, 0xa0)] {
                if mask & bit != 0 && !(vk == 0xa5 && m == 0xa2) && held.remove(&m.to_string()) {
                    items.push(key(m, true));
                }
            }
        }
        if vk == 0xa5 && held.remove(ALTGR_CTRL) && held.remove(&0xa2u16.to_string()) {
            items.push(key(0xa2, true));
        }
        if vk == 0xa2 {
            held.remove(ALTGR_CTRL);
        }
        return Ok(items);
    }
    let paired_ctrl = vk == 0xa5
        && !held.contains(&0xa2u16.to_string())
        && !held.contains(&0xa3u16.to_string())
        && mask & 10 == 0;
    let mut modifiers = if paired_ctrl { vec![0xa2] } else { vec![] };
    for (bit, m) in [(2, 0xa2), (8, 0xa2), (4, 0xa4), (1, 0xa0)] {
        if mask & bit != 0 && m != vk && !modifiers.contains(&m) && !held.contains(&m.to_string()) {
            modifiers.push(m);
        }
    }
    let mut items: Vec<_> = modifiers.iter().map(|m| key(*m, false)).collect();
    items.push(key(vk, false));
    if phase == "press" {
        items.push(key(vk, true));
        items.extend(modifiers.iter().rev().map(|m| key(*m, true)));
    } else {
        // An explicit Control down takes ownership from the AltGr pair.
        if vk == 0xa2 {
            held.remove(ALTGR_CTRL);
        }
        held.insert(vk.to_string());
        held.extend(modifiers.iter().map(u16::to_string));
        if vk == 0xa5 && modifiers.contains(&0xa2) {
            held.insert(ALTGR_CTRL.to_string());
        }
    }
    Ok(items)
}
fn unicode(text: &str) -> Vec<INPUT> {
    text.encode_utf16()
        .flat_map(|unit| {
            [false, true].map(|up| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wScan: unit,
                        dwFlags: KEYEVENTF_UNICODE
                            | if up {
                                KEYEVENTF_KEYUP
                            } else {
                                KEYBD_EVENT_FLAGS(0)
                            },
                        ..Default::default()
                    },
                },
            })
        })
        .collect()
}

/// A long text action has cancellation points between characters. Each
/// character's UTF-16 pairs stay together; an entire paste is never queued
/// into the OS after only one permission check.
fn send_text_guarded(
    text: &str,
    permission: &InputPermission,
    mut send: impl FnMut(&[INPUT]) -> Result<(), String>,
) -> Result<(), String> {
    for character in text.chars() {
        permission.check()?;
        send(&unicode(character.encode_utf8(&mut [0; 4])))?;
    }
    Ok(())
}

impl Helper {
    pub(crate) async fn release_held(&self, st: &mut InputState) {
        for b in std::mem::take(&mut st.held_buttons) {
            let _ = win::button(&b, false);
        }
        let items: Vec<_> = std::mem::take(&mut st.held_keys)
            .iter()
            .filter_map(|k| k.parse().ok())
            .map(|k| key(k, true))
            .collect();
        let _ = win::send(&items);
    }
    pub(crate) async fn deliver(
        &self,
        st: &mut InputState,
        action: &Value,
        result: &mut Value,
    ) -> Result<(), String> {
        st.check()?;
        let kind = action["type"].as_str().ok_or("invalid_input")?;
        if kind == "releaseAll" {
            self.release_held(st).await;
            return Ok(());
        }
        if kind != "clipboard" && win::secure_desktop_active() {
            return Err("input_desktop_unavailable".into());
        }
        let id = st
            .source
            .as_ref()
            .and_then(|s| s["id"].as_str())
            .ok_or("source_not_selected")?;
        // Final drag position is applied only after exact-source validation.
        // The owned button is released even if validation/movement fails.
        if kind == "pointer" && action["phase"] == "up" {
            let b = action["button"].as_str().unwrap_or("left");
            if st.held_buttons.contains(b) {
                let moved = (|| -> Result<(), String> {
                    let s = source(Some(id))?;
                    let p = map_point(
                        action["x"].as_f64().ok_or("invalid_input")?,
                        action["y"].as_f64().ok_or("invalid_input")?,
                        s.origin.0,
                        s.origin.1,
                        s.width,
                        s.height,
                    )?;
                    under(&s, p)?;
                    st.check()?;
                    win::move_to(p).map_err(String::from)
                })();
                let released = win::button(b, false);
                if released.is_ok() {
                    st.held_buttons.remove(b);
                }
                moved?;
                released?;
                return Ok(());
            }
        }
        if kind == "key" && key_ignored_here(action["key"].as_str().unwrap_or("")) {
            return Ok(());
        }
        if kind == "key" && action["phase"] == "up" {
            let vk = virtual_key(action["key"].as_str().ok_or("invalid_input")?)?;
            if st.held_keys.contains(&vk.to_string()) {
                let items = key_inputs(
                    vk,
                    "up",
                    action["modifiers"].as_i64().unwrap_or(0),
                    &mut st.held_keys,
                )?;
                st.check()?;
                win::send(&items)?;
                return Ok(());
            }
        }
        let s = source(Some(id))?;
        // Restore a minimized selected window, then map input to where it now is.
        st.check()?;
        let s = if kind != "clipboard" && win::restore_if_minimized(&s) {
            source(Some(id))?
        } else {
            s
        };
        match kind {
            "pointer" => {
                let b = action["button"].as_str().unwrap_or("left");
                if !["left", "middle", "right"].contains(&b) {
                    return Err("invalid_input".into());
                }
                let phase = action["phase"].as_str().ok_or("invalid_input")?;
                if !["up", "down", "move"].contains(&phase) {
                    return Err("invalid_input".into());
                }
                let p = map_point(
                    action["x"].as_f64().ok_or("invalid_input")?,
                    action["y"].as_f64().ok_or("invalid_input")?,
                    s.origin.0,
                    s.origin.1,
                    s.width,
                    s.height,
                )?;
                if st.held_buttons.is_empty() {
                    focus(&s, true, st)?;
                }
                under(&s, p)?;
                st.check()?;
                win::move_to(p)?;
                if phase == "down" {
                    st.held_buttons.insert(b.into());
                    st.check()?;
                    win::button(b, true)?;
                } else if phase == "up" {
                    win::button(b, false)?;
                    st.held_buttons.remove(b);
                }
            }
            "scroll" => {
                let (dx, dy) = (
                    action["deltaX"].as_f64().ok_or("invalid_input")?,
                    action["deltaY"].as_f64().ok_or("invalid_input")?,
                );
                if !dx.is_finite() || !dy.is_finite() || dx.abs() > 4096.0 || dy.abs() > 4096.0 {
                    return Err("invalid_input".into());
                }
                if st.held_buttons.is_empty() {
                    focus(&s, true, st)?;
                }
                if dx != 0.0 || dy != 0.0 {
                    under(&s, win::cursor()?)?;
                }
                st.check()?;
                win::wheel(dx, dy)?;
            }
            "key" => {
                let vk = virtual_key(action["key"].as_str().ok_or("invalid_input")?)?;
                let phase = action["phase"].as_str().ok_or("invalid_input")?;
                focus(&s, true, st)?;
                let items = key_inputs(
                    vk,
                    phase,
                    action["modifiers"].as_i64().unwrap_or(0),
                    &mut st.held_keys,
                )?;
                st.check()?;
                win::send(&items)?;
            }
            "text" => {
                let text = action["text"].as_str().unwrap_or("");
                if text.chars().count() > 4096 {
                    return Err("invalid_input".into());
                }
                if !text.is_empty() {
                    focus(&s, true, st)?;
                }
                send_text_guarded(text, &st.permission(), |items| {
                    win::send(items).map_err(String::from)
                })?;
            }
            "clipboard" => match action["operation"].as_str() {
                Some("copyToPhone") => {
                    result["clipboardText"] = json!(clipboard_read()?);
                }
                Some("pasteFromPhone") => {
                    let text = action["text"].as_str().unwrap_or("");
                    if text.len() > 65536 {
                        return Err("invalid_input".into());
                    }
                    focus(&s, true, st)?;
                    clipboard_write(text, st)?;
                    focus(&s, false, st)?;
                    st.check()?;
                    win::send(&[
                        key(0xa2, false),
                        key(0x56, false),
                        key(0x56, true),
                        key(0xa2, true),
                    ])?;
                }
                _ => return Err("invalid_input".into()),
            },
            "releaseAll" => self.release_held(st).await,
            _ => return Err("invalid_input".into()),
        }
        Ok(())
    }
}

struct Clipboard;
impl Clipboard {
    fn open() -> Result<Self, String> {
        use windows::Win32::System::DataExchange::OpenClipboard;
        for _ in 0..20 {
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(Self);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err("clipboard_unavailable".into())
    }
}
impl Drop for Clipboard {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::System::DataExchange::CloseClipboard() };
    }
}
fn clipboard_read() -> Result<String, String> {
    use windows::Win32::{
        Foundation::HGLOBAL,
        System::{
            DataExchange::GetClipboardData,
            Memory::{GlobalLock, GlobalSize, GlobalUnlock},
        },
    };
    let _clipboard = Clipboard::open()?;
    let Ok(handle) = (unsafe { GetClipboardData(13) }) else {
        return Ok(String::new());
    };
    let h = HGLOBAL(handle.0);
    let size = unsafe { GlobalSize(h) };
    if size > 131074 {
        return Err("clipboard_too_large".into());
    }
    let ptr = unsafe { GlobalLock(h) }.cast::<u16>();
    if ptr.is_null() {
        return Err("clipboard_unavailable".into());
    }
    let data = unsafe { std::slice::from_raw_parts(ptr, size / 2) };
    let len = data.iter().position(|v| *v == 0).unwrap_or(data.len());
    let text = String::from_utf16_lossy(&data[..len]);
    let _ = unsafe { GlobalUnlock(h) };
    if text.len() > 65536 {
        return Err("clipboard_too_large".into());
    }
    Ok(text)
}
fn clipboard_write(text: &str, permission: &InputState) -> Result<(), String> {
    use windows::Win32::{
        Foundation::{GlobalFree, HANDLE},
        System::{
            DataExchange::{EmptyClipboard, SetClipboardData},
            Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
        },
    };
    let data: Vec<_> = text.encode_utf16().chain(Some(0)).collect();
    let _clipboard = Clipboard::open()?;
    permission.check()?;
    unsafe {
        EmptyClipboard().map_err(|_| "clipboard_unavailable")?;
        let h = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2).map_err(|_| "clipboard_unavailable")?;
        let ptr = GlobalLock(h).cast::<u16>();
        if ptr.is_null() {
            let _ = GlobalFree(Some(h));
            return Err("clipboard_unavailable".into());
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        let _ = GlobalUnlock(h);
        if let Err(error) = permission.check() {
            let _ = GlobalFree(Some(h));
            return Err(error);
        }
        if SetClipboardData(13, Some(HANDLE(h.0))).is_err() {
            let _ = GlobalFree(Some(h));
            return Err("clipboard_unavailable".into());
        }
    }
    Ok(())
}

type CaptureError = Box<dyn std::error::Error + Send + Sync>;
struct CaptureContext {
    job: CaptureJob,
    /// Why capture ended on its own; the first reason wins.
    ended: std::sync::OnceLock<&'static str>,
}
struct Handler {
    context: Arc<CaptureContext>,
    pacer: crate::pacing::FramePacer,
    #[cfg(feature = "parity-tests")]
    profile: CaptureProfile,
}
#[cfg(feature = "parity-tests")]
#[derive(Default)]
struct CaptureProfile {
    start: Option<Instant>,
    callbacks: u64,
    skipped: u64,
    accepted: u64,
    map_us: u128,
    convert_us: u128,
    submit_us: u128,
}
impl GraphicsCaptureApiHandler for Handler {
    type Flags = Arc<CaptureContext>;
    type Error = CaptureError;
    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            context: ctx.flags,
            pacer: crate::pacing::FramePacer::new(Instant::now()),
            #[cfg(feature = "parity-tests")]
            profile: CaptureProfile::default(),
        })
    }
    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.context.job.stop.load(Ordering::SeqCst) {
            control.stop();
            return Ok(());
        }
        let now = Instant::now();
        #[cfg(feature = "parity-tests")]
        {
            self.profile.start.get_or_insert(now);
            self.profile.callbacks += 1;
        }
        if !self
            .pacer
            .admit(now, self.context.job.live.fps.load(Ordering::SeqCst))
        {
            #[cfg(feature = "parity-tests")]
            {
                self.profile.skipped += 1;
            }
            return Ok(());
        }
        let mapped = frame.buffer();
        let Ok(mut buffer) = mapped else {
            let _ = self.context.ended.set("capture_failed");
            control.stop();
            return Ok(());
        };
        let (w, h, pitch) = (
            buffer.width() & !1,
            buffer.height() & !1,
            buffer.row_pitch(),
        );
        if w < 2 || h < 2 || w > 8192 || h > 8192 {
            let _ = self.context.ended.set("capture_failed");
            control.stop();
            return Ok(());
        }
        let mut i420 = I420Buffer::new(w, h);
        #[cfg(feature = "parity-tests")]
        let mapped_at = Instant::now();
        let (sy, su, sv) = i420.strides();
        let (y, u, v) = i420.data_mut();
        let popup_pixels = (self.context.job.selected_source["kind"] == "window")
            .then(|| source(self.context.job.selected_source["id"].as_str()).ok())
            .flatten()
            .filter(|s| s.width & !1 == w && s.height & !1 == h)
            .and_then(|s| win::composite_popups(&s, buffer.as_raw_buffer(), pitch));
        yuv_helper::argb_to_i420(
            popup_pixels
                .as_deref()
                .unwrap_or_else(|| buffer.as_raw_buffer()),
            pitch,
            y,
            sy,
            u,
            su,
            v,
            sv,
            w as i32,
            h as i32,
        );
        #[cfg(feature = "parity-tests")]
        let converted_at = Instant::now();
        self.context.job.submit(i420);
        #[cfg(feature = "parity-tests")]
        {
            self.profile.accepted += 1;
            self.profile.map_us += mapped_at.duration_since(now).as_micros();
            self.profile.convert_us += converted_at.duration_since(mapped_at).as_micros();
            self.profile.submit_us += converted_at.elapsed().as_micros();
            let elapsed = self.profile.start.unwrap().elapsed().as_secs_f64();
            if elapsed >= 1.0 {
                crate::bench(json!({"k":"wgc_profile", "seconds":elapsed,
                    "callbacks":self.profile.callbacks,"skipped":self.profile.skipped,
                    "accepted":self.profile.accepted,"mapUs":self.profile.map_us,
                    "convertUs":self.profile.convert_us,"submitUs":self.profile.submit_us}));
                self.profile = CaptureProfile::default();
            }
        }
        Ok(())
    }
    fn on_closed(&mut self) -> Result<(), Self::Error> {
        let _ = self.context.ended.set("source_not_found");
        Ok(())
    }
}
fn settings<T: TryInto<GraphicsCaptureItemType>, F>(item: T, context: F) -> Settings<F, T> {
    Settings::new(
        item,
        // Explicit cursor and border choices need Windows 10 builds 19041 and
        // 20348; earlier builds refuse them. The defaults (cursor shown) work
        // everywhere WGC does.
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        context,
    )
}
pub(super) fn capture_loop(job: &CaptureJob) -> Result<(), String> {
    let s = source(job.selected_source["id"].as_str())?;
    let context = Arc::new(CaptureContext {
        job: job.clone(),
        ended: std::sync::OnceLock::new(),
    });
    let capture = match s.hwnd {
        Some(h) => Handler::start_free_threaded(settings(
            Window::from_raw_hwnd(h as *mut _),
            context.clone(),
        )),
        None => Handler::start_free_threaded(settings(
            Monitor::from_index(s.monitor).map_err(|_| "capture_unavailable")?,
            context.clone(),
        )),
    }
    .map_err(|_| "capture_unavailable")?;
    while !job.stop.load(Ordering::SeqCst)
        && context.ended.get().is_none()
        && !capture.is_finished()
    {
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = capture.stop();
    if job.stop.load(Ordering::SeqCst) {
        return Ok(());
    }
    // A failed frame can be the first sign of a closed window; only a source
    // that is really gone reports as removed.
    if source(job.selected_source["id"].as_str()).is_err() {
        return Err("source_not_found".into());
    }
    Err(context
        .ended
        .get()
        .copied()
        .unwrap_or("capture_failed")
        .into())
}

/// Standard Win32 edits and caret-bearing text controls report focus without
/// reading their contents. Unsupported custom controls keep manual keyboard.
pub(super) fn text_input_focused(source: &Value) -> bool {
    let Ok(s) = self::source(source["id"].as_str()) else {
        return false;
    };
    let foreground = unsafe { GetForegroundWindow() };
    if let Some(selected) = s.hwnd {
        if win::root(foreground) != selected
            && unsafe { GetAncestor(foreground, GA_ROOTOWNER).0 as isize } != selected
        {
            return false;
        }
    }
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetGUIThreadInfo(0, &mut info) }.is_err() || info.hwndFocus.0.is_null() {
        return false;
    }
    let mut name = [0u16; 128];
    let count = unsafe { GetClassNameW(info.hwndFocus, &mut name) };
    let name = String::from_utf16_lossy(&name[..count.max(0) as usize]).to_ascii_lowercase();
    let edit = name == "edit" || name.starts_with("richedit");
    if edit && unsafe { GetWindowLongW(info.hwndFocus, GWL_STYLE) } & 0x0800 != 0 {
        return false;
    } // ES_READONLY
    edit || !info.hwndCaret.0.is_null()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};

    unsafe extern "system" fn test_window_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        DefWindowProcW(h, m, w, l)
    }

    /// The canonical key-name table (crates/wonderd/RPC.md, "Computer key names").
    #[test]
    fn canonical_key_names_map_to_virtual_keys() {
        for (name, expected) in [
            ("insert", 0x2d),
            ("printscreen", 0x2c),
            ("menu", 0x5d),
            ("f13", 0x7c),
            ("f14", 0x7d),
            ("f15", 0x7e),
            ("f16", 0x7f),
            ("f17", 0x80),
            ("f18", 0x81),
            ("f19", 0x82),
            ("volumeup", 0xaf),
            ("volumedown", 0xae),
            ("mute", 0xad),
            ("playpause", 0xb3),
            ("next", 0xb0),
            ("previous", 0xb1),
            ("rightoption", 0xa5),
        ] {
            assert_eq!(virtual_key(name).unwrap(), expected, "{name}");
            assert_eq!(
                virtual_key(&name.to_uppercase()).unwrap(),
                expected,
                "{name}"
            );
            let extended =
                [0x2d, 0x2c, 0x5d, 0xa5, 0xad, 0xae, 0xaf, 0xb0, 0xb1, 0xb3].contains(&expected);
            for up in [false, true] {
                let input = unsafe { key(expected, up).Anonymous.ki };
                assert_eq!(input.wVk.0, expected);
                assert_eq!(
                    (input.dwFlags & KEYEVENTF_EXTENDEDKEY).0 != 0,
                    extended,
                    "{name}"
                );
                assert_eq!((input.dwFlags & KEYEVENTF_KEYUP).0 != 0, up, "{name}");
            }
        }
        assert!(virtual_key("f20").is_err());
        assert!(virtual_key("brightnessup").is_err());
        assert!(key_ignored_here("BrightnessUp") && key_ignored_here("brightnessdown"));
        assert!(!key_ignored_here("mute"));
    }

    #[test]
    fn altgr_releases_only_its_paired_control() {
        let encode = |items: Vec<INPUT>| -> Vec<(u16, bool)> {
            items
                .iter()
                .map(|input| unsafe {
                    (
                        input.Anonymous.ki.wVk.0,
                        (input.Anonymous.ki.dwFlags & KEYEVENTF_KEYUP).0 != 0,
                    )
                })
                .collect()
        };
        for (control, mask) in [
            (None, 0),
            (None, 2),
            (None, 8),
            (Some(0xa2u16), 0),
            (Some(0xa2), 2),
            (Some(0xa2), 8),
            (Some(0xa3), 0),
        ] {
            let mut held: BTreeSet<String> = control.iter().map(u16::to_string).collect();
            let mut sent = Vec::new();
            for phase in ["down", "down", "up"] {
                sent.extend(encode(key_inputs(0xa5, phase, mask, &mut held).unwrap()));
            }
            let mut expected = Vec::new();
            if control.is_none() {
                expected.push((0xa2, false));
            }
            expected.extend([(0xa5, false), (0xa5, false), (0xa5, true)]);
            if control.is_none() {
                expected.push((0xa2, true));
            }
            assert_eq!(sent, expected);
            assert_eq!(held, control.iter().map(u16::to_string).collect());
        }
        let mut held = BTreeSet::new();
        assert_eq!(
            encode(key_inputs(0xa5, "press", 0, &mut held).unwrap()),
            [(0xa2, false), (0xa5, false), (0xa5, true), (0xa2, true)]
        );
        assert!(held.is_empty());
        key_inputs(0xa5, "down", 0, &mut held).unwrap();
        key_inputs(0xa2, "down", 0, &mut held).unwrap();
        assert_eq!(
            encode(key_inputs(0xa5, "up", 0, &mut held).unwrap()),
            [(0xa5, true)]
        );
        assert_eq!(held, BTreeSet::from([0xa2u16.to_string()]));
    }

    #[test]
    fn revoking_a_long_text_action_stops_before_the_next_character() {
        let (generation, receiver) = tokio::sync::watch::channel(0);
        let permission = InputPermission {
            execution: receiver,
            generation: 0,
            deadline: None,
        };
        let mut scans = Vec::new();
        let result = send_text_guarded("a🙂b", &permission, |items| {
            scans.extend(items.iter().map(|item| unsafe { item.Anonymous.ki.wScan }));
            generation.send_replace(1);
            Ok(())
        });
        assert_eq!(result.unwrap_err(), "lease_not_active");
        assert_eq!(scans, vec![b'a' as u16, b'a' as u16]);
    }

    #[test]
    fn capture_asks_only_for_what_every_windows_10_build_supports() {
        // windows-capture refuses a non-default choice on a build without it, so
        // one would make every capture fail on older Windows 10.
        let s = settings(Window::from_raw_hwnd(std::ptr::null_mut()), ());
        assert_eq!(s.cursor_capture(), CursorCaptureSettings::Default);
        assert_eq!(s.draw_border(), DrawBorderSettings::Default);
        assert_eq!(s.secondary_window(), SecondaryWindowSettings::Default);
        assert_eq!(
            s.minimum_update_interval(),
            MinimumUpdateIntervalSettings::Default
        );
        assert_eq!(s.dirty_region(), DirtyRegionSettings::Default);
    }

    struct TestWindow(HWND, Option<std::thread::JoinHandle<()>>);
    impl TestWindow {
        fn new(title: PCWSTR) -> Self {
            // A real top-level window with a message loop. The Static control
            // class is click-through and cannot exercise the occlusion guard.
            static CLASS: std::sync::Once = std::sync::Once::new();
            CLASS.call_once(|| unsafe {
                assert_ne!(
                    RegisterClassW(&WNDCLASSW {
                        lpfnWndProc: Some(test_window_proc),
                        lpszClassName: w!("CupolaCaptureTestWindow"),
                        ..Default::default()
                    }),
                    0
                );
            });
            let title = title.as_ptr() as usize;
            let (ready, window) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || unsafe {
                let h = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("CupolaCaptureTestWindow"),
                    PCWSTR(title as *const u16),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    120,
                    120,
                    640,
                    420,
                    None,
                    None,
                    None,
                    None,
                )
                .expect("create a test window");
                let _ = ShowWindow(h, SW_SHOWNORMAL);
                let _ = ShowWindow(h, SW_SHOWNORMAL);
                ready.send(h.0 as isize).unwrap();
                let mut message = MSG::default();
                while IsWindow(Some(h)).as_bool() && GetMessageW(&mut message, None, 0, 0).0 > 0 {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            });
            Self(win::hwnd(window.recv().unwrap()), Some(thread))
        }
        fn raw(&self) -> isize {
            self.0 .0 as isize
        }
    }
    impl Drop for TestWindow {
        fn drop(&mut self) {
            unsafe {
                let _ = PostMessageW(
                    Some(self.0),
                    WM_CLOSE,
                    Default::default(),
                    Default::default(),
                );
            }
            self.1.take().unwrap().join().unwrap();
        }
    }

    #[test]
    #[ignore = "run in the interactive console session; requires Windows Graphics Capture"]
    fn default_capture_delivers_a_native_window_frame() {
        initialize();
        let selected = TestWindow::new(w!("Cupola capture acceptance"));
        let exe = win::window_exe(selected.raw()).unwrap();
        let id = format!("app:{exe}#{}", selected.raw());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let helper = runtime.block_on(async { crate::tests::host() });
        let selected_source = resolve_source(Some(&id)).unwrap();
        runtime.block_on(async {
            helper.state.lock().await.source = Some(selected_source.clone());
        });
        let live = Arc::new(Live::default());
        live.fps.store(30, Ordering::SeqCst);
        let stop = Arc::new(AtomicBool::new(false));
        let job = CaptureJob {
            helper,
            runtime: runtime.handle().clone(),
            source: runtime.block_on(async {
                NativeVideoSource::new(
                    VideoResolution {
                        width: 640,
                        height: 420,
                    },
                    false,
                )
            }),
            live: live.clone(),
            stop: stop.clone(),
            selected_source,
            session_id: None,
            generation: 0,
        };
        let capture = std::thread::spawn(move || capture_loop(&job));
        let deadline = Instant::now() + Duration::from_secs(10);
        while live.sequence.load(Ordering::SeqCst) == 0
            && !capture.is_finished()
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(25));
        }
        stop.store(true, Ordering::SeqCst);
        let result = capture.join().expect("capture thread");
        assert!(result.is_ok(), "capture result: {result:?}");
        let frames = live.sequence.load(Ordering::SeqCst);
        assert!(
            frames > 0,
            "the production capture loop must deliver a frame"
        );
        let frame = live.last_frame.lock().unwrap();
        let frame = frame.as_ref().unwrap();
        assert!(frame["sourceWidth"].as_u64().unwrap() > 0);
        assert!(frame["sourceHeight"].as_u64().unwrap() > 0);
        eprintln!("native capture: frames={frames}, metadata={frame}");
    }

    #[test]
    #[ignore = "run in the interactive console session; service sessions cannot show windows"]
    fn a_minimized_selected_window_is_resolved_directly_and_restored() {
        initialize();
        // Many windows of the same app, the selected one created first so it
        // sits below the rest.
        let selected = TestWindow::new(w!("Cupola selected window"));
        let others: Vec<_> = (0..40)
            .map(|_| TestWindow::new(w!("Cupola other window")))
            .collect();
        let raw = selected.raw();
        let exe = win::window_exe(raw).expect("own exe");
        let id = format!("app:{exe}#{raw}");
        let open = source(Some(&id)).expect("an open window resolves");
        unsafe {
            let _ = ShowWindow(selected.0, SW_MINIMIZE);
            assert!(IsIconic(selected.0).as_bool());
        }
        // Discovery no longer offers it, but control still knows it.
        assert!(!win::list_windows(std::slice::from_ref(&exe), false)
            .iter()
            .any(|s| s["id"].as_str() == Some(id.as_str())));
        let minimized = source(Some(&id)).expect("a minimized selected window is not closed");
        assert_eq!(minimized.hwnd, Some(raw));
        assert!(win::restore_if_minimized(&minimized));
        assert!(unsafe { !IsIconic(selected.0).as_bool() });
        assert!(!win::restore_if_minimized(&minimized));
        let restored = source(Some(&id)).expect("a restored window resolves");
        assert_eq!((restored.width, restored.height), (open.width, open.height));

        // Exercise the production caller as well: it must restore before mapping
        // coordinates, retain the lease, and leave other minimized windows alone.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let helper = crate::tests::host();
            let identity = json!({"sessionID":"fixture","generation":1,"sourceID":id,
                "geometryRevision":1,"leaseID":"lease","requestID":"request"});
            {
                let mut st = helper.state.lock().await;
                st.session_id = Some("fixture".into());
                st.generation = 1;
                st.state = "capturing";
                st.source = Some(resolve_source(Some(&id)).unwrap());
            }
            assert!(matches!(
                helper.consent(&identity).await.unwrap(),
                Reply::Now(_)
            ));
            assert_eq!(
                helper.handle("control.activate", &identity).await.unwrap()["activated"],
                true
            );
            unsafe {
                let _ = ShowWindow(selected.0, SW_MINIMIZE);
                let _ = ShowWindow(others[0].0, SW_MINIMIZE);
            }
            let mut input = identity.clone();
            input["sequence"] = json!(1);
            input["actions"] = json!([{"type":"pointer","phase":"move","x":0.5,"y":0.5}]);
            let delivered = helper.handle("control.input", &input).await;
            let delivered = delivered.unwrap();
            assert_eq!(delivered["accepted"], true, "{delivered}");
            assert!(unsafe { !IsIconic(selected.0).as_bool() });
            assert!(unsafe { IsIconic(others[0].0).as_bool() });
            let restored = source(Some(&id)).unwrap();
            let expected = map_point(
                0.5,
                0.5,
                restored.origin.0,
                restored.origin.1,
                restored.width,
                restored.height,
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while win::cursor().unwrap() != expected && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(
                win::cursor().unwrap(),
                expected,
                "input uses restored geometry"
            );
            assert_eq!(
                helper.handle("control.heartbeat", &identity).await.unwrap()["renewed"],
                true
            );

            let mut other = input.clone();
            other["sourceID"] = json!(format!("app:{exe}#{}", others[0].raw()));
            assert_eq!(
                helper.handle("control.input", &other).await.unwrap_err(),
                "invalid_capture_session"
            );
            assert!(unsafe { IsIconic(others[0].0).as_bool() });

            // While dragging, another window covering the selected source must
            // still refuse input. No physical button-down is needed for the guard.
            let _cover = TestWindow::new(w!("Cupola covering window"));
            helper.input.lock().await.held_buttons.insert("left".into());
            input["sequence"] = json!(2);
            assert_eq!(
                helper.handle("control.input", &input).await.unwrap_err(),
                "window_covered"
            );
            assert!(helper.state.lock().await.lease.is_none());
        });
        // Another app's name, or the window once closed, is not this source.
        let other = format!("app:notepad.exe#{raw}");
        assert_eq!(source(Some(&other)).unwrap_err(), "source_not_found");
        drop(selected);
        assert_eq!(source(Some(&id)).unwrap_err(), "source_not_found");
    }
}

#[cfg(test)]
mod dock_tests {
    use super::*;
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    /// Exercise the shell's real shortcut format and Unicode paths, with only
    /// owned files. No shell launch, focus, input or interactive desktop needed.
    #[test]
    fn dock_shortcut_fixture_uses_shell_resolution_and_shell_icons() {
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let fixture = Fixture(std::env::temp_dir().join(
            format!("K3aW-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()),
        ));
        std::fs::create_dir_all(&fixture.0).unwrap();
        let program = fixture.0.join("日本語.exe");
        let system = std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap()).join("System32");
        std::fs::copy(system.join("cmd.exe"), &program).unwrap();
        let wide = |path: &std::path::Path| {
            use std::os::windows::ffi::OsStrExt;
            path.as_os_str()
                .encode_wide()
                .chain([0])
                .collect::<Vec<_>>()
        };
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            struct Apartment;
            impl Drop for Apartment {
                fn drop(&mut self) {
                    unsafe { CoUninitialize() }
                }
            }
            let _apartment = Apartment;
            for (name, target) in [
                ("A B.lnk", Some(program.as_path())),
                ("A-B.lnk", Some(system.join("cmd.exe")).as_deref()),
                ("日本語.lnk", Some(program.as_path())),
                ("Document.lnk", Some(fixture.0.join("notes.txt")).as_deref()),
                ("Namespace.lnk", None),
            ] {
                let link: IShellLinkW =
                    CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
                if let Some(target) = target {
                    link.SetPath(PCWSTR(wide(target).as_ptr())).unwrap();
                }
                // Persisted arguments must survive: launch targets the .lnk,
                // rather than replacing it with its resolved executable.
                let args: Vec<_> = "/c echo K3aW".encode_utf16().chain([0]).collect();
                link.SetArguments(PCWSTR(args.as_ptr())).unwrap();
                let file: IPersistFile = link.cast().unwrap();
                file.Save(PCWSTR(wide(&fixture.0.join(name)).as_ptr()), true)
                    .unwrap();
            }
        }
        std::fs::write(fixture.0.join("Broken.lnk"), b"not a shortcut").unwrap();
        let pins = dock_pins_at(&fixture.0);
        assert_eq!(
            pins.iter().map(|pin| pin.name.as_str()).collect::<Vec<_>>(),
            ["A B", "A-B", "日本語"]
        );
        assert_eq!(pins[0].program, program.to_str().unwrap());
        assert_ne!(pins[0].id, pins[1].id);
        for pin in &pins {
            assert_eq!(
                windows_dock::launch(&pin.id, &pins, &[]),
                Some(windows_dock::Launch::Shortcut(pin.shortcut.as_str()))
            );
        }
        let answer = windows_dock::build(&pins, &[], win::file_icon);
        let icon = answer["pinned"][0]["iconPNG"]
            .as_str()
            .expect("shell file icon");
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(icon)
            .unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(icon.len() <= 3072);
    }
}
