//! Win32 pieces of the D prototype: sources, window discovery and pointer
//! input through SendInput. Mirrors the Python helper's rules for window
//! sources: pointer and wheel go only where the source window is on top.

use super::{HelperError, Source};
use serde_json::{json, Value};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, GetStockObject, COLOR_BTNFACE, DEFAULT_GUI_FONT, HBRUSH,
    HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub fn dpi_aware() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub(super) fn hwnd(h: isize) -> HWND {
    HWND(h as *mut _)
}

pub fn window_rect(h: isize) -> Result<(i32, i32, u32, u32), HelperError> {
    let mut r = RECT::default();
    unsafe {
        if DwmGetWindowAttribute(
            hwnd(h),
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut _ as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
            && GetWindowRect(hwnd(h), &mut r).is_err()
        {
            return Err(HelperError("source_not_found"));
        }
    }
    Ok((
        r.left,
        r.top,
        (r.right - r.left).max(0) as u32,
        (r.bottom - r.top).max(0) as u32,
    ))
}

pub fn window_exe(h: isize) -> Option<String> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd(h), Some(&mut pid));
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
        .is_ok();
        let _ = CloseHandle(process);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        Some(path.rsplit('\\').next().unwrap_or(&path).to_lowercase())
    }
}

pub fn window_title(h: isize) -> String {
    unsafe {
        let mut buf = [0u16; 512];
        let n = GetWindowTextW(hwnd(h), &mut buf);
        super::super::bound_title(&String::from_utf16_lossy(&buf[..n.max(0) as usize]))
    }
}

fn cloaked(h: isize) -> bool {
    let mut v = 0u32;
    unsafe {
        let _ = DwmGetWindowAttribute(hwnd(h), DWMWA_CLOAKED, &mut v as *mut _ as *mut _, 4);
    }
    v != 0
}

fn monitors() -> Vec<RECT> {
    unsafe extern "system" fn visit(
        m: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> windows::core::BOOL {
        let list = &mut *(data.0 as *mut Vec<RECT>);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(m, &mut info).as_bool() {
            list.push(info.rcMonitor);
        }
        true.into()
    }
    let mut list: Vec<RECT> = vec![];
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(visit),
            LPARAM(&mut list as *mut _ as isize),
        );
    }
    list
}

pub fn display_source(index: usize) -> Result<Source, HelperError> {
    let list = monitors();
    let r = list
        .get(index.wrapping_sub(1))
        .ok_or(HelperError("source_not_found"))?;
    let (w, h) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
    Ok(Source {
        id: format!("display:{index}"),
        kind: "display",
        title: format!("Display {index}"),
        app_id: None,
        hwnd: None,
        width: w,
        height: h,
        origin: (r.left, r.top),
        monitor: index,
    })
}

pub(super) fn root(h: HWND) -> isize {
    unsafe { GetAncestor(h, GA_ROOT).0 as isize }
}

pub fn require_under(s: &Source, point: (i32, i32)) -> Result<(), HelperError> {
    if s.hwnd.is_none() {
        return Ok(());
    }
    let at = unsafe {
        WindowFromPoint(POINT {
            x: point.0,
            y: point.1,
        })
    };
    if !accepts_hit(s, root(at), point) {
        crate::trace(&format!(
            "covered point={point:?} hit={} root={} source={:?} origin={:?} size={}x{}",
            at.0 as isize,
            root(at),
            s.hwnd,
            s.origin,
            s.width,
            s.height
        ));
        return Err(HelperError("window_covered"));
    }
    Ok(())
}

fn accepts_hit(s: &Source, hit: isize, point: (i32, i32)) -> bool {
    let Some(h) = s.hwnd else {
        return true;
    };
    let owned = unsafe { GetAncestor(hwnd(hit), GA_ROOTOWNER).0 as isize == h };
    let inside = point.0 >= s.origin.0
        && point.1 >= s.origin.1
        && point.0 < s.origin.0 + s.width as i32
        && point.1 < s.origin.1 + s.height as i32;
    inside && (hit == h || (owned && popup_was_captured(h, hit, point)))
}

pub fn cursor() -> Result<(i32, i32), HelperError> {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p).map_err(|_| HelperError("input_blocked"))? };
    Ok((p.x, p.y))
}

/// UAC consent, Ctrl-Alt-Del or the lock screen own input: GetCursorPos
/// fails with access denied (the Python helper's measured signal).
pub fn secure_desktop_active() -> bool {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p).is_err() }
}

pub(super) fn send(inputs: &[INPUT]) -> Result<(), HelperError> {
    if let Some(Some(owner)) = INPUT_OWNER.get() {
        return owner_send(&mut owner.lock().unwrap(), inputs);
    }
    #[cfg(test)]
    return raw_send(inputs);
    #[cfg(not(test))]
    Err(HelperError("input_blocked"))
}
#[cfg(test)]
fn raw_send(inputs: &[INPUT]) -> Result<(), HelperError> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(HelperError("input_blocked"));
    }
    Ok(())
}

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

pub fn move_to(p: (i32, i32)) -> Result<(), HelperError> {
    let (left, top, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let nx = ((p.0 - left) as f64 * 65535.0 / (w - 1).max(1) as f64).round() as i32;
    let ny = ((p.1 - top) as f64 * 65535.0 / (h - 1).max(1) as f64).round() as i32;
    send(&[mouse(
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        nx,
        ny,
        0,
    )])
}

pub fn button(name: &str, down: bool) -> Result<(), HelperError> {
    let flag = match (name, down) {
        ("left", true) => MOUSEEVENTF_LEFTDOWN,
        ("left", false) => MOUSEEVENTF_LEFTUP,
        ("right", true) => MOUSEEVENTF_RIGHTDOWN,
        ("right", false) => MOUSEEVENTF_RIGHTUP,
        ("middle", true) => MOUSEEVENTF_MIDDLEDOWN,
        _ => MOUSEEVENTF_MIDDLEUP,
    };
    send(&[mouse(flag, 0, 0, 0)])
}

pub fn wheel(dx: f64, dy: f64) -> Result<(), HelperError> {
    let mut items = vec![];
    for (delta, flag, sign) in [(dy, MOUSEEVENTF_WHEEL, -1), (dx, MOUSEEVENTF_HWHEEL, 1)] {
        let clicks = ((delta.abs() / 40.0).round() as i32)
            .max(if delta != 0.0 { 1 } else { 0 })
            .min(20);
        if clicks > 0 {
            items.push(mouse(
                flag,
                0,
                0,
                sign * if delta > 0.0 { 1 } else { -1 } * clicks * 120,
            ));
        }
    }
    send(&items)
}

/// Reads what the shared window rules need; the rules themselves live in
/// `shareable_window` so discovery and input apply the same ones.
fn window_traits(h: HWND) -> Option<super::super::WindowTraits> {
    let raw = h.0 as isize;
    unsafe {
        let mut class = [0u16; 256];
        let n = GetClassNameW(h, &mut class);
        let ex_style = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        let mut transparent = false;
        if ex_style & 0x00080000 != 0 {
            let (mut alpha, mut flags) = (255u8, 0u32);
            transparent = window_transparency(raw, std::ptr::null_mut(), &mut alpha, &mut flags)
                != 0
                && flags & 2 != 0
                && alpha == 0;
        }
        Some(super::super::WindowTraits {
            visible: IsWindowVisible(h).as_bool(),
            minimized: IsIconic(h).as_bool(),
            cloaked: cloaked(raw),
            owned: GetAncestor(h, GA_ROOTOWNER).0 as isize != raw,
            class: String::from_utf16_lossy(&class[..n.max(0) as usize]),
            ex_style,
            transparent,
            exe: window_exe(raw)?,
            title: window_title(raw),
        })
    }
}

/// The selected window, looked up by its handle alone: no enumeration, icons or
/// cap. It must still be the same app's shareable top-level window; a minimized
/// one is still itself.
pub fn selected_window(id: &str, app: &str, h: isize) -> Result<Source, HelperError> {
    let shareable = unsafe { IsWindow(Some(hwnd(h))).as_bool() }
        && window_traits(hwnd(h)).is_some_and(|t| {
            t.exe == app.to_lowercase() && super::super::shareable_window(&t, true)
        });
    if !shareable {
        return Err(HelperError("source_not_found"));
    }
    let (x, y, w, hgt) = window_rect(h)?;
    Ok(Source {
        id: id.into(),
        kind: "window",
        title: window_title(h),
        app_id: Some(app.into()),
        hwnd: Some(h),
        width: w,
        height: hgt,
        origin: (x, y),
        monitor: 0,
    })
}

/// Restores the selected window if it is minimized, and only that window.
pub fn restore_if_minimized(s: &Source) -> bool {
    let Some(h) = s.hwnd else { return false };
    unsafe {
        if !IsIconic(hwnd(h)).as_bool() {
            return false;
        }
        let _ = ShowWindow(hwnd(h), SW_RESTORE);
    }
    true
}

pub(super) fn list_windows(apps: &[String], all: bool) -> Vec<Value> {
    enumerate_windows(apps, all, false)
}

/// The same enumeration order as discovery, retaining minimized apps for the
/// dock's running indicator and restore action. No discovery behavior changes.
pub(super) fn list_dock_windows() -> Vec<Value> {
    enumerate_windows(&[], true, true)
}

fn enumerate_windows(apps: &[String], all: bool, include_minimized: bool) -> Vec<Value> {
    struct Ctx<'a> {
        apps: &'a [String],
        all: bool,
        include_minimized: bool,
        found: Vec<Value>,
    }
    unsafe extern "system" fn visit(h: HWND, data: LPARAM) -> windows::core::BOOL {
        let ctx = &mut *(data.0 as *mut Ctx);
        if ctx.found.len() >= 32 {
            return false.into();
        }
        let raw = h.0 as isize;
        // Cheap checks first: most top-level windows are hidden.
        if !IsWindowVisible(h).as_bool() || (!ctx.include_minimized && IsIconic(h).as_bool()) {
            return true.into();
        }
        let Some(t) = window_traits(h) else {
            return true.into();
        };
        if !super::super::shareable_window(&t, ctx.include_minimized)
            || !(ctx.all || ctx.apps.contains(&t.exe))
        {
            return true.into();
        }
        let (exe, title) = (t.exe, t.title);
        if let Ok((_, _, w, hh)) = window_rect(raw) {
            if (ctx.include_minimized || (w >= 80 && hh >= 60)) && raw > 0 && raw <= 0xFFFF_FFFF {
                let mut source = json!({"kind": "window", "id": format!("app:{exe}#{raw}"), "appId": exe,
                    "name": title, "appName": exe.trim_end_matches(".exe"), "windowTitle": title, "width":w, "height":hh});
                if let Some(icon) = app_icon(raw, &exe) {
                    source["iconPNG"] = json!(icon);
                }
                ctx.found.push(source);
            }
        }
        true.into()
    }
    let mut ctx = Ctx {
        apps,
        all,
        include_minimized,
        found: vec![],
    };
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.found
}

// The icon APIs are borrowed handles; only our DC and DIB are owned.
#[link(name = "user32")]
unsafe extern "system" {
    #[link_name = "SendMessageTimeoutW"]
    fn icon_message(
        h: isize,
        msg: u32,
        w: usize,
        l: isize,
        flags: u32,
        timeout: u32,
        result: *mut usize,
    ) -> isize;
    #[link_name = "GetClassLongPtrW"]
    fn class_icon(h: isize, index: i32) -> usize;
    #[link_name = "LoadIconW"]
    fn fallback_icon(module: isize, name: usize) -> isize;
    #[link_name = "DrawIconEx"]
    fn draw_icon(
        dc: isize,
        x: i32,
        y: i32,
        icon: isize,
        w: i32,
        h: i32,
        step: u32,
        brush: isize,
        flags: u32,
    ) -> i32;
    #[link_name = "GetLayeredWindowAttributes"]
    fn window_transparency(h: isize, color: *mut u32, alpha: *mut u8, flags: *mut u32) -> i32;
}
#[link(name = "gdi32")]
unsafe extern "system" {
    #[link_name = "CreateCompatibleDC"]
    fn icon_dc(dc: isize) -> isize;
    #[link_name = "CreateDIBSection"]
    fn icon_dib(
        dc: isize,
        info: *const DibInfo,
        usage: u32,
        bits: *mut *mut u8,
        section: isize,
        offset: u32,
    ) -> isize;
    #[link_name = "SelectObject"]
    fn select_icon(dc: isize, object: isize) -> isize;
    #[link_name = "DeleteObject"]
    fn delete_icon_object(object: isize) -> i32;
    #[link_name = "DeleteDC"]
    fn delete_icon_dc(dc: isize) -> i32;
}
#[repr(C)]
struct DibInfo {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bits: u16,
    compression: u32,
    image_size: u32,
    x: i32,
    y: i32,
    used: u32,
    important: u32,
}
/// Draws an icon handle into the fixed 20 px PNG the wire carries.
unsafe fn hicon_png(icon: usize) -> Option<String> {
    unsafe {
        let dc = icon_dc(0);
        if dc == 0 {
            return None;
        }
        let info = DibInfo {
            size: 40,
            width: 20,
            height: -20,
            planes: 1,
            bits: 32,
            compression: 0,
            image_size: 1600,
            x: 0,
            y: 0,
            used: 0,
            important: 0,
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = icon_dib(dc, &info, 0, &mut bits, 0, 0);
        if bitmap == 0 || bits.is_null() {
            if bitmap != 0 {
                delete_icon_object(bitmap);
            }
            delete_icon_dc(dc);
            return None;
        }
        let previous = select_icon(dc, bitmap);
        std::ptr::write_bytes(bits, 0, 1600);
        let drawn = draw_icon(dc, 0, 0, icon as isize, 20, 20, 0, 0, 3) != 0;
        let pixels = std::slice::from_raw_parts(bits, 1600).to_vec();
        let legacy = pixels.iter().skip(3).step_by(4).all(|a| *a == 0);
        let mask = if legacy {
            std::ptr::write_bytes(bits, 255, 1600);
            if draw_icon(dc, 0, 0, icon as isize, 20, 20, 0, 0, 1) != 0 {
                Some(std::slice::from_raw_parts(bits, 1600).to_vec())
            } else {
                None
            }
        } else {
            None
        };
        select_icon(dc, previous);
        delete_icon_object(bitmap);
        delete_icon_dc(dc);
        if !drawn {
            return None;
        }
        let mut rgba = Vec::with_capacity(1600);
        for (i, pixel) in pixels.chunks_exact(4).enumerate() {
            let mut alpha = pixel[3];
            if legacy {
                alpha = mask.as_ref().map(|m| 255 - m[i * 4]).unwrap_or(255);
            }
            for channel in [pixel[2], pixel[1], pixel[0]] {
                rgba.push(if !legacy && alpha > 0 && alpha < 255 {
                    ((channel as u32 * 255 + alpha as u32 / 2) / alpha as u32).min(255) as u8
                } else {
                    channel
                });
            }
            rgba.push(alpha);
        }
        super::super::icon_png(&rgba)
    }
}

#[link(name = "shell32")]
unsafe extern "system" {
    #[link_name = "SHGetFileInfoW"]
    fn shell_file_info(
        path: *const u16,
        attributes: u32,
        info: *mut ShellFileInfo,
        size: u32,
        flags: u32,
    ) -> usize;
    #[link_name = "ShellExecuteW"]
    fn shell_execute(
        window: isize,
        verb: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show: i32,
    ) -> isize;
}
#[link(name = "user32")]
unsafe extern "system" {
    #[link_name = "DestroyIcon"]
    fn destroy_icon(icon: isize) -> i32;
}
#[repr(C)]
struct ShellFileInfo {
    icon: isize,
    index: i32,
    attributes: u32,
    display_name: [u16; 260],
    type_name: [u16; 80],
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Each successful COM initialization owns exactly one matching uninitialize.
struct ShellApartment(bool);
impl ShellApartment {
    fn new() -> Option<Self> {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result.is_ok() {
            Some(Self(true))
        } else if result.0 == 0x80010106u32 as i32 {
            // RPC_E_CHANGED_MODE: COM already initialized.
            Some(Self(false))
        } else {
            None
        }
    }
}
impl Drop for ShellApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

/// Let the shell resolve Unicode, environment paths and shortcut variants.
/// UWP/namespace pins have no local executable path and are deliberately skipped.
pub fn shortcut_program(path: &std::path::Path) -> Option<String> {
    use windows::core::Interface;
    use windows::Win32::System::Com::{
        CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    let _apartment = ShellApartment::new()?;
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let file: IPersistFile = link.cast().ok()?;
        use std::os::windows::ffi::OsStrExt;
        let path: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
        file.Load(PCWSTR(path.as_ptr()), STGM_READ).ok()?;
        let mut target = [0u16; 32768];
        link.GetPath(&mut target, std::ptr::null_mut(), 0).ok()?;
        let end = target.iter().position(|c| *c == 0)?;
        let target = String::from_utf16(&target[..end]).ok()?;
        let path = std::path::Path::new(&target);
        let local_program = path.is_absolute()
            && target.as_bytes().get(1) == Some(&b':')
            && path.extension().is_some_and(|ext| {
                ["exe", "bat", "cmd"]
                    .iter()
                    .any(|e| ext.eq_ignore_ascii_case(e))
            });
        local_program.then_some(target)
    }
}

/// Focus a freshly resolved application window from the helper, without
/// starting or forwarding the action to the input-owner process.
pub fn focus_app_window(source: &Source) -> bool {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    let Some(raw) = source.hwnd else { return false };
    if secure_desktop_active() || window_exe(raw) != source.app_id {
        return false;
    }
    unsafe {
        let window = hwnd(raw);
        let owns_focus = || {
            let foreground = GetForegroundWindow();
            foreground == window || GetAncestor(foreground, GA_ROOTOWNER) == window
        };
        if owns_focus() {
            return true;
        }
        if IsIconic(window).as_bool() {
            let _ = ShowWindow(window, SW_RESTORE);
        }
        let _ = SetForegroundWindow(window);
        if owns_focus() {
            return true;
        }
        let ours = GetCurrentThreadId();
        let theirs = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let attached =
            theirs != 0 && theirs != ours && AttachThreadInput(ours, theirs, true).as_bool();
        let _ = BringWindowToTop(window);
        let _ = SetForegroundWindow(window);
        if attached {
            let _ = AttachThreadInput(ours, theirs, false);
        }
        owns_focus()
    }
}

/// The shell's icon for a program or shortcut file, as the wire's 20 px PNG.
pub fn file_icon(path: &str) -> Option<String> {
    unsafe {
        let mut info: ShellFileInfo = std::mem::zeroed();
        let path = wide(path);
        // SHGFI_ICON | SHGFI_LARGEICON.
        let ok = shell_file_info(
            path.as_ptr(),
            0,
            &mut info,
            std::mem::size_of::<ShellFileInfo>() as u32,
            0x100,
        );
        if ok == 0 || info.icon == 0 {
            return None;
        }
        let png = hicon_png(info.icon as usize);
        destroy_icon(info.icon);
        png
    }
}

/// Opens a shortcut or program the way Explorer does. Called from the helper
/// process, never the input-owner process.
pub fn open_file(path: &str) -> bool {
    unsafe {
        let path = wide(path);
        // Values above 32 mean success.
        shell_execute(
            0,
            wide("open").as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        ) > 32
    }
}

fn app_icon(h: isize, exe: &str) -> Option<String> {
    use std::{
        sync::{Mutex, OnceLock},
        time::{Duration, Instant},
    };
    type Item = (String, usize, Instant, String);
    static CACHE: OnceLock<Mutex<Vec<Item>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(Vec::new()));
    unsafe {
        let mut result = 0usize;
        icon_message(h, 0x7f, 1, 0, 2, 50, &mut result);
        let icon = if result != 0 {
            result
        } else {
            class_icon(h, -14)
        };
        let icon = if icon != 0 {
            icon
        } else {
            fallback_icon(0, 32512) as usize
        };
        if icon == 0 {
            return None;
        }
        if let Some((_, _, _, value)) = cache.lock().unwrap().iter().find(|(app, id, at, _)| {
            app == exe && *id == icon && at.elapsed() < Duration::from_secs(60)
        }) {
            return Some(value.clone());
        }
        let value = hicon_png(icon)?;
        let mut entries = cache.lock().unwrap();
        entries.retain(|(app, id, _, _)| app != exe || *id != icon);
        if entries.len() >= 64 {
            entries.remove(0);
        }
        entries.push((exe.into(), icon, Instant::now(), value.clone()));
        Some(value)
    }
}

// ---- On-screen panels: the Take control question and the notice with Stop.

const PANEL_CLASS: PCWSTR = windows::core::w!("CupolaControlPanel");
/// Posted by the helper to close a panel without an answer.
const PANEL_CLOSE: u32 = WM_APP + 1;
const PANEL_BUTTON: usize = 100;
const PANEL_WIDTH: i32 = 480;

struct PanelState {
    choice: Option<tokio::sync::oneshot::Sender<usize>>,
    buttons: usize,
}

/// Runs one always-on-top panel on the calling thread until it closes. Its
/// handle goes to `ready` (`None` when it cannot be shown); the button the
/// person presses goes to `choice`. Escape or closing the window counts as
/// the last button: "Don't Allow" or "Stop". `ask` brings it to the front;
/// otherwise it never takes focus from the window being controlled.
pub fn run_panel(
    text: &'static str,
    buttons: &'static [&'static str],
    ask: bool,
    choice: tokio::sync::oneshot::Sender<usize>,
    ready: std::sync::mpsc::Sender<Option<isize>>,
) {
    let Some(window) = (unsafe { create_panel(text, buttons, ask, choice) }) else {
        let _ = ready.send(None);
        return;
    };
    if ready.send(Some(window.0 as isize)).is_err() {
        // The helper stopped waiting: never leave an unowned panel up.
        unsafe {
            let _ = DestroyWindow(window);
        }
    }
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // Tab moves between the buttons; Escape answers with the last one.
            if !IsDialogMessageW(window, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

/// Closes a panel `run_panel` showed, without an answer.
pub fn close_panel(window: isize) {
    unsafe {
        let _ = PostMessageW(Some(hwnd(window)), PANEL_CLOSE, WPARAM(0), LPARAM(0));
    }
}

unsafe fn create_panel(
    text: &str,
    buttons: &[&str],
    ask: bool,
    choice: tokio::sync::oneshot::Sender<usize>,
) -> Option<HWND> {
    let instance: HINSTANCE = GetModuleHandleW(None).ok()?.into();
    static REGISTER: std::sync::Once = std::sync::Once::new();
    REGISTER.call_once(|| {
        let class = WNDCLASSW {
            lpfnWndProc: Some(panel_proc),
            hInstance: instance,
            lpszClassName: PANEL_CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _),
            ..Default::default()
        };
        RegisterClassW(&class);
    });
    let mut style = WS_EX_TOPMOST | WS_EX_TOOLWINDOW;
    if !ask {
        style |= WS_EX_NOACTIVATE;
    }
    let x = (GetSystemMetrics(SM_CXSCREEN) - PANEL_WIDTH).max(0) / 2;
    let title = wide("Cupola");
    // A caption without a close box: the buttons are the only answers.
    let window = CreateWindowExW(
        style,
        PANEL_CLASS,
        PCWSTR(title.as_ptr()),
        WS_POPUP | WS_CAPTION | WS_BORDER,
        x,
        24,
        PANEL_WIDTH,
        150,
        None,
        None,
        Some(instance),
        None,
    )
    .ok()?;
    let state = Box::new(PanelState {
        choice: Some(choice),
        buttons: buttons.len(),
    });
    SetWindowLongPtrW(window, GWLP_USERDATA, Box::into_raw(state) as isize);
    let font = WPARAM(GetStockObject(DEFAULT_GUI_FONT).0 as usize);
    let label = wide(text);
    let mut children = vec![CreateWindowExW(
        WINDOW_EX_STYLE(0),
        windows::core::w!("STATIC"),
        PCWSTR(label.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        16,
        14,
        PANEL_WIDTH - 32,
        48,
        Some(window),
        None,
        Some(instance),
        None,
    )];
    for (index, name) in buttons.iter().enumerate() {
        let name = wide(name);
        let from_right = (buttons.len() - index) as i32;
        children.push(CreateWindowExW(
            WINDOW_EX_STYLE(0),
            windows::core::w!("BUTTON"),
            PCWSTR(name.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
            PANEL_WIDTH - 16 - from_right * 128,
            70,
            120,
            30,
            Some(window),
            Some(HMENU((PANEL_BUTTON + index) as *mut _)),
            Some(instance),
            None,
        ));
    }
    for child in &children {
        match child {
            Ok(child) => {
                SendMessageW(*child, WM_SETFONT, Some(font), Some(LPARAM(1)));
            }
            Err(_) => {
                let _ = DestroyWindow(window);
                return None;
            }
        }
    }
    let _ = ShowWindow(window, if ask { SW_SHOW } else { SW_SHOWNOACTIVATE });
    if ask {
        let _ = SetForegroundWindow(window);
    }
    Some(window)
}

unsafe extern "system" fn panel_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut PanelState;
    let answer = |button: usize| {
        if let Some(state) = state.as_mut() {
            if let Some(choice) = state.choice.take() {
                let _ = choice.send(button);
            }
        }
    };
    let last = state.as_ref().map_or(0, |s| s.buttons.saturating_sub(1));
    match message {
        WM_COMMAND => {
            let id = wparam.0 & 0xffff;
            let button = if id == IDCANCEL.0 as usize {
                Some(last)
            } else {
                id.checked_sub(PANEL_BUTTON).filter(|b| *b <= last)
            };
            if let Some(button) = button {
                answer(button);
                let _ = DestroyWindow(window);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            answer(last);
            let _ = DestroyWindow(window);
            LRESULT(0)
        }
        PANEL_CLOSE => {
            let _ = DestroyWindow(window);
            LRESULT(0)
        }
        WM_DESTROY => {
            if !state.is_null() {
                SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                drop(Box::from_raw(state));
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

// The input owner is launched by wonderd outside the capture helper's job.
// Killing that job closes its socket; this process releases only successfully
// injected downs before exiting. A wedged helper loses ownership after 2 s.
// It has no capture or host credentials and accepts one authenticated client.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
static INPUT_OWNER: OnceLock<Option<Arc<Mutex<TcpStream>>>> = OnceLock::new();
static INPUT_OWNER_LIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) fn connect_input_owner() -> bool {
    INPUT_OWNER
        .get_or_init(|| {
            let address = std::env::var("WONDER_INPUT_OWNER").ok()?;
            let address: std::net::SocketAddr = address.parse().ok()?;
            if !address.ip().is_loopback() {
                return None;
            }
            let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1)).ok()?;
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .ok()?;
            stream
                .set_write_timeout(Some(Duration::from_millis(500)))
                .ok()?;
            stream.set_nodelay(true).ok()?;
            let token = std::env::var("WONDER_COMPUTER_USE_HANDSHAKE").ok()?;
            if token.is_empty() || token.len() > 160 {
                return None;
            }
            stream.write_all(&[token.len() as u8]).ok()?;
            stream.write_all(token.as_bytes()).ok()?;
            let mut ready = [0];
            stream.read_exact(&mut ready).ok()?;
            if ready != [1] {
                return None;
            }
            INPUT_OWNER_LIVE.store(true, std::sync::atomic::Ordering::SeqCst);
            let stream = Arc::new(Mutex::new(stream));
            let weak = Arc::downgrade(&stream);
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(250));
                let Some(stream) = weak.upgrade() else {
                    return;
                };
                if owner_send(&mut stream.lock().unwrap(), &[]).is_err() {
                    INPUT_OWNER_LIVE.store(false, std::sync::atomic::Ordering::SeqCst);
                    let _ = stream.lock().unwrap().shutdown(std::net::Shutdown::Both);
                    return;
                }
            });
            Some(stream)
        })
        .is_some()
        && INPUT_OWNER_LIVE.load(std::sync::atomic::Ordering::SeqCst)
}

fn owner_send(stream: &mut TcpStream, inputs: &[INPUT]) -> Result<(), HelperError> {
    if inputs.len() > 16384 {
        return Err(HelperError("invalid_input"));
    }
    stream
        .write_all(&(inputs.len() as u32).to_le_bytes())
        .map_err(|_| HelperError("input_blocked"))?;
    let mut bytes = Vec::with_capacity(inputs.len() * 28);
    for item in inputs {
        let fields = unsafe {
            if item.r#type == INPUT_KEYBOARD {
                let k = item.Anonymous.ki;
                [1, k.wVk.0 as u32, k.wScan as u32, k.dwFlags.0, 0, 0, 0]
            } else if item.r#type == INPUT_MOUSE {
                let m = item.Anonymous.mi;
                [0, m.dx as u32, m.dy as u32, m.dwFlags.0, m.mouseData, 0, 0]
            } else {
                return Err(HelperError("invalid_input"));
            }
        };
        for field in fields {
            bytes.extend_from_slice(&field.to_le_bytes());
        }
    }
    stream
        .write_all(&bytes)
        .map_err(|_| HelperError("input_blocked"))?;
    let mut sent = [0; 4];
    stream
        .read_exact(&mut sent)
        .map_err(|_| HelperError("input_blocked"))?;
    if u32::from_le_bytes(sent) as usize != inputs.len() {
        return Err(HelperError("input_blocked"));
    }
    Ok(())
}

#[derive(Default)]
struct Injected {
    keys: std::collections::BTreeMap<(u16, u16, u32), INPUT>,
    buttons: std::collections::BTreeMap<u32, INPUT>,
}
impl Injected {
    fn record(&mut self, items: &[INPUT]) {
        for item in items {
            unsafe {
                if item.r#type == INPUT_KEYBOARD {
                    let k = item.Anonymous.ki;
                    let id = (k.wVk.0, k.wScan, k.dwFlags.0 & !KEYEVENTF_KEYUP.0);
                    if k.dwFlags.contains(KEYEVENTF_KEYUP) {
                        self.keys.remove(&id);
                    } else {
                        let mut up = *item;
                        up.Anonymous.ki.dwFlags |= KEYEVENTF_KEYUP;
                        self.keys.insert(id, up);
                    }
                } else if item.r#type == INPUT_MOUSE {
                    let m = item.Anonymous.mi;
                    for (down, up) in [
                        (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
                        (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
                        (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
                    ] {
                        if m.dwFlags.contains(up) {
                            self.buttons.remove(&down.0);
                        }
                        if m.dwFlags.contains(down) {
                            self.buttons.insert(down.0, mouse(up, 0, 0, 0));
                        }
                    }
                }
            }
        }
    }
    fn releases(&self) -> Vec<INPUT> {
        self.buttons
            .values()
            .chain(self.keys.values())
            .copied()
            .collect()
    }
}
struct InputOwner<F: FnMut(&[INPUT]) -> u32> {
    injected: Injected,
    send: F,
}
impl<F: FnMut(&[INPUT]) -> u32> Drop for InputOwner<F> {
    fn drop(&mut self) {
        (self.send)(&self.injected.releases());
    }
}

fn input_loop(mut stream: TcpStream, send: impl FnMut(&[INPUT]) -> u32, idle: Duration) {
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(idle));
    let mut owner = InputOwner {
        injected: Injected::default(),
        send,
    };
    loop {
        let mut size = [0; 4];
        if stream.read_exact(&mut size).is_err() {
            return;
        }
        let size = u32::from_le_bytes(size) as usize;
        if size > 16384 {
            return;
        }
        let mut bytes = vec![0; size * 28];
        if stream.read_exact(&mut bytes).is_err() {
            return;
        }
        let mut items = Vec::with_capacity(size);
        for record in bytes.as_chunks::<28>().0 {
            let f: Vec<u32> = record
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| u32::from_le_bytes(*b))
                .collect();
            let item = match f[0] {
                1 => INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(f[1] as u16),
                            wScan: f[2] as u16,
                            dwFlags: KEYBD_EVENT_FLAGS(f[3]),
                            ..Default::default()
                        },
                    },
                },
                0 => mouse(
                    MOUSE_EVENT_FLAGS(f[3]),
                    f[1] as i32,
                    f[2] as i32,
                    f[4] as i32,
                ),
                _ => return,
            };
            items.push(item);
        }
        let sent = (owner.send)(&items);
        owner.injected.record(&items[..(sent as usize).min(size)]);
        if stream.write_all(&sent.to_le_bytes()).is_err() {
            return;
        }
    }
}

pub(super) fn input_owner() {
    dpi_aware();
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
        return;
    };
    let Ok(token) = std::env::var("WONDER_COMPUTER_USE_HANDSHAKE") else {
        return;
    };
    if token.is_empty() || token.len() > 160 {
        return;
    }
    let _ = listener.set_nonblocking(true);
    println!("{}", listener.local_addr().unwrap());
    let _ = std::io::stdout().flush();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let Ok((mut stream, _)) = listener.accept() else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        // Windows inherits the listener's nonblocking mode on accept.
        // Restore blocking I/O before handshake or a later message can race it.
        if stream.set_nonblocking(false).is_err() {
            continue;
        }
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
        let mut length = [0];
        if stream.read_exact(&mut length).is_err() || length[0] as usize != token.len() {
            continue;
        }
        let mut received = vec![0; token.len()];
        if stream.read_exact(&mut received).is_err() || received != token.as_bytes() {
            continue;
        }
        if stream.write_all(&[1]).is_err() {
            return;
        }
        input_loop(
            stream,
            |items| unsafe { SendInput(items, std::mem::size_of::<INPUT>() as i32) },
            Duration::from_secs(2),
        );
        return;
    }
}

/// Only owned popup pixels composited into the most recent selected-window
/// frame may receive input. Relationship alone does not expose another window.
type VisiblePopup = (isize, (i32, i32, u32, u32));
static POPUPS: Mutex<Option<(isize, Instant, Vec<VisiblePopup>)>> = Mutex::new(None);
fn popup_was_captured(selected: isize, hit: isize, p: (i32, i32)) -> bool {
    POPUPS
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|(source, at, popups)| {
            *source == selected
                && at.elapsed() < Duration::from_millis(500)
                && popups.iter().any(|(h, (x, y, w, hg))| {
                    *h == hit
                        && p.0 >= *x
                        && p.1 >= *y
                        && p.0 < *x + *w as i32
                        && p.1 < *y + *hg as i32
                })
        })
}

pub(super) fn composite_popups(source: &Source, pixels: &[u8], pitch: u32) -> Option<Vec<u8>> {
    use windows::Win32::Graphics::Gdi::{BitBlt, GetDC, ReleaseDC, SRCCOPY};
    let selected = source.hwnd?;
    unsafe extern "system" fn visit(h: HWND, l: LPARAM) -> windows::core::BOOL {
        let (selected, popups) = &mut *(l.0 as *mut (isize, Vec<isize>));
        if popups.len() < 32
            && h.0 as isize != *selected
            && IsWindowVisible(h).as_bool()
            && GetAncestor(h, GA_ROOTOWNER).0 as isize == *selected
        {
            popups.push(h.0 as isize);
        }
        true.into()
    }
    let mut windows = (selected, Vec::new());
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut windows as *mut _ as isize));
    }
    let mut output = None;
    let mut captured = Vec::new();
    // EnumWindows is top to bottom; compose in painter's order.
    for popup in windows.1.into_iter().rev() {
        let Ok((px, py, pw, ph)) = window_rect(popup) else {
            continue;
        };
        let x = px.max(source.origin.0);
        let y = py.max(source.origin.1);
        let right = (px + pw as i32).min(source.origin.0 + source.width as i32);
        let bottom = (py + ph as i32).min(source.origin.1 + source.height as i32);
        if right <= x || bottom <= y {
            continue;
        }
        let (w, h) = ((right - x) as u32, (bottom - y) as u32);
        if w > 8192 || h > 8192 {
            continue;
        }
        unsafe {
            let from = GetDC(Some(hwnd(popup)));
            if from.is_invalid() {
                continue;
            }
            let dc = icon_dc(0);
            if dc == 0 {
                ReleaseDC(Some(hwnd(popup)), from);
                continue;
            }
            let info = DibInfo {
                size: 40,
                width: w as i32,
                height: -(h as i32),
                planes: 1,
                bits: 32,
                compression: 0,
                image_size: w * h * 4,
                x: 0,
                y: 0,
                used: 0,
                important: 0,
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = icon_dib(dc, &info, 0, &mut bits, 0, 0);
            if bitmap == 0 || bits.is_null() {
                if bitmap != 0 {
                    delete_icon_object(bitmap);
                }
                delete_icon_dc(dc);
                ReleaseDC(Some(hwnd(popup)), from);
                continue;
            }
            let previous = select_icon(dc, bitmap);
            std::ptr::write_bytes(bits, 0, (w * h * 4) as usize);
            let ok = BitBlt(
                HDC(dc as *mut _),
                0,
                0,
                w as i32,
                h as i32,
                Some(from),
                x - px,
                y - py,
                SRCCOPY,
            )
            .is_ok();
            if ok {
                let popup_pixels =
                    std::slice::from_raw_parts(bits.cast::<u8>(), (w * h * 4) as usize);
                let target = output.get_or_insert_with(|| pixels.to_vec());
                for row in 0..h as usize {
                    let offset = (y - source.origin.1) as usize * pitch as usize
                        + row * pitch as usize
                        + (x - source.origin.0) as usize * 4;
                    let count = w as usize * 4;
                    if offset + count <= target.len() {
                        target[offset..offset + count]
                            .copy_from_slice(&popup_pixels[row * count..(row + 1) * count]);
                    }
                }
                captured.push((popup, (x, y, w, h)));
            }
            select_icon(dc, previous);
            delete_icon_object(bitmap);
            delete_icon_dc(dc);
            ReleaseDC(Some(hwnd(popup)), from);
        }
    }
    *POPUPS.lock().unwrap() = Some((selected, Instant::now(), captured));
    output
}

#[cfg(test)]
mod input_owner_tests {
    use super::*;
    fn vk(vk: u16, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    },
                    ..Default::default()
                },
            },
        }
    }
    #[test]
    fn lost_helper_and_wedged_helper_release_only_recorded_downs() {
        for disconnect in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let (server, _) = listener.accept().unwrap();
            server.set_nonblocking(true).unwrap();
            let batches = Arc::new(Mutex::new(Vec::<Vec<(u32, u16, u32)>>::new()));
            let sink = batches.clone();
            let thread = std::thread::spawn(move || {
                input_loop(
                    server,
                    move |items| {
                        sink.lock().unwrap().push(
                            items
                                .iter()
                                .map(|i| unsafe {
                                    if i.r#type == INPUT_KEYBOARD {
                                        (1, i.Anonymous.ki.wVk.0, i.Anonymous.ki.dwFlags.0)
                                    } else {
                                        (0, 0, i.Anonymous.mi.dwFlags.0)
                                    }
                                })
                                .collect(),
                        );
                        items.len() as u32
                    },
                    Duration::from_secs(10),
                )
            });
            owner_send(
                &mut client,
                &[
                    vk(0xa0, false),
                    vk(0x41, false),
                    vk(0x41, true),
                    mouse(MOUSEEVENTF_LEFTDOWN, 0, 0, 0),
                ],
            )
            .unwrap();
            if disconnect {
                drop(client);
            }
            let start = Instant::now();
            thread.join().unwrap();
            assert!(start.elapsed() < Duration::from_secs(15));
            let batches = batches.lock().unwrap();
            assert_eq!(batches.len(), 2);
            assert_eq!(
                batches[1],
                vec![(0, 0, MOUSEEVENTF_LEFTUP.0), (1, 0xa0, KEYEVENTF_KEYUP.0)]
            );
        }
    }
    #[test]
    fn an_owned_popup_requires_recent_captured_pixels_and_a_point_inside_the_selected_view() {
        use windows::core::w;
        unsafe {
            let create = |owner: Option<HWND>| {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("Static"),
                    w!("Cupola menu fixture"),
                    WS_POPUP,
                    0,
                    0,
                    100,
                    100,
                    owner,
                    None,
                    None,
                    None,
                )
                .unwrap()
            };
            let selected = create(None);
            let popup = create(Some(selected));
            let unrelated = create(None);
            assert_eq!(
                GetWindowThreadProcessId(selected, None),
                GetWindowThreadProcessId(unrelated, None)
            );
            let s = Source {
                id: "fixture".into(),
                kind: "window",
                title: "fixture".into(),
                app_id: None,
                hwnd: Some(selected.0 as isize),
                width: 100,
                height: 100,
                origin: (0, 0),
                monitor: 0,
            };
            *POPUPS.lock().unwrap() = None;
            assert!(!accepts_hit(&s, popup.0 as isize, (20, 20)));
            *POPUPS.lock().unwrap() = Some((
                selected.0 as isize,
                Instant::now(),
                vec![(popup.0 as isize, (10, 10, 50, 50))],
            ));
            assert!(accepts_hit(&s, popup.0 as isize, (20, 20)));
            assert!(!accepts_hit(&s, popup.0 as isize, (80, 80)));
            assert!(!accepts_hit(&s, popup.0 as isize, (120, 20)));
            assert!(
                !accepts_hit(&s, unrelated.0 as isize, (20, 20)),
                "same process is not ownership"
            );
            *POPUPS.lock().unwrap() = None;
            let _ = DestroyWindow(popup);
            let _ = DestroyWindow(unrelated);
            let _ = DestroyWindow(selected);
        }
    }

    #[test]
    fn partial_send_tracks_only_the_successful_prefix() {
        let mut injected = Injected::default();
        let batch = [vk(0xa0, false), vk(0x41, false)];
        injected.record(&batch[..1]);
        let releases = injected.releases();
        assert_eq!(releases.len(), 1);
        unsafe {
            assert_eq!(releases[0].Anonymous.ki.wVk.0, 0xa0);
            assert_eq!(releases[0].Anonymous.ki.dwFlags, KEYEVENTF_KEYUP);
        }
        injected.record(&[vk(0xa0, true)]);
        assert!(injected.releases().is_empty());
    }
}
