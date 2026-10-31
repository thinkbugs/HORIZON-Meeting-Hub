use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::Serialize;
use tauri::Manager;

mod firewall;
mod media_http;
mod media_library;
mod media_stage;
mod receivers;
mod sender;
mod snapdrop_server;

#[cfg(windows)]
mod autostart;

use receivers::Receivers;
use sender::Sender;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportedApplication {
    name: String,
    path: String,
    icon: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SystemStatus {
    battery_percent: Option<u8>,
    charging: bool,
}

fn state_file(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("无法定位应用配置目录：{error}"))?;
    std::fs::create_dir_all(&dir).map_err(|error| format!("无法创建配置目录：{error}"))?;
    Ok(dir.join("launcher-state.json"))
}

#[tauri::command]
fn load_state(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let path = state_file(&app)?;
    match std::fs::read_to_string(&path) {
        Ok(raw) if !raw.trim().is_empty() => Ok(Some(raw)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读取配置失败：{error}")),
    }
}

#[tauri::command]
fn save_state(app: tauri::AppHandle, state: String) -> Result<(), String> {
    serde_json::from_str::<serde_json::Value>(&state)
        .map_err(|error| format!("配置数据无效：{error}"))?;
    let path = state_file(&app)?;
    std::fs::write(&path, state.as_bytes()).map_err(|error| format!("保存配置失败：{error}"))
}

/// Welcome photos and songs live beside the settings file so that changing an
/// unrelated setting never re-serialises megabytes of base64 on the webview.
fn media_file(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let path = state_file(app)?.with_file_name("welcome-media.json");
    Ok(path)
}

#[tauri::command]
fn load_welcome_media(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let path = media_file(&app)?;
    match std::fs::read_to_string(&path) {
        Ok(raw) if !raw.trim().is_empty() => Ok(Some(raw)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读取欢迎媒体失败：{error}")),
    }
}

#[tauri::command]
fn save_welcome_media(app: tauri::AppHandle, payload: String) -> Result<(), String> {
    serde_json::from_str::<serde_json::Value>(&payload)
        .map_err(|error| format!("欢迎媒体数据无效：{error}"))?;
    let path = media_file(&app)?;
    std::fs::write(&path, payload.as_bytes()).map_err(|error| format!("保存欢迎媒体失败：{error}"))
}

fn file_stem_name(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[cfg(windows)]
fn shell_display_name(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
    use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_DISPLAYNAME};

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut info = SHFILEINFOW::default();
    let result = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_DISPLAYNAME,
        )
    };
    if result == 0 {
        return None;
    }
    let end = info
        .szDisplayName
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(info.szDisplayName.len());
    let name = String::from_utf16_lossy(&info.szDisplayName[..end])
        .trim()
        .to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(not(windows))]
fn shell_display_name(_path: &Path) -> Option<String> {
    None
}

#[cfg(windows)]
fn file_description(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(
            PCWSTR(wide.as_ptr()),
            None,
            size,
            data.as_mut_ptr() as *mut _,
        )
        .ok()?;

        let mut translation_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut translation_size = 0u32;
        let translation_query: Vec<u16> = "\\VarFileInfo\\Translation\0".encode_utf16().collect();
        if !VerQueryValueW(
            data.as_ptr() as *const _,
            PCWSTR(translation_query.as_ptr()),
            &mut translation_ptr,
            &mut translation_size,
        )
        .as_bool()
            || translation_ptr.is_null()
            || translation_size < 4
        {
            return None;
        }

        let pair = std::slice::from_raw_parts(translation_ptr as *const u16, 2);
        let query = format!(
            "\\StringFileInfo\\{:04x}{:04x}\\FileDescription\0",
            pair[0], pair[1]
        );
        let query_wide: Vec<u16> = query.encode_utf16().collect();
        let mut description_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut description_size = 0u32;
        if !VerQueryValueW(
            data.as_ptr() as *const _,
            PCWSTR(query_wide.as_ptr()),
            &mut description_ptr,
            &mut description_size,
        )
        .as_bool()
            || description_ptr.is_null()
            || description_size == 0
        {
            return None;
        }

        let offset = (description_ptr as usize).saturating_sub(data.as_ptr() as usize);
        let available_chars = data.len().saturating_sub(offset) / 2;
        if available_chars == 0 {
            return None;
        }
        let raw = std::slice::from_raw_parts(
            description_ptr as *const u16,
            available_chars.min(1024),
        );
        let end = raw.iter().position(|character| *character == 0)?;
        let text = String::from_utf16_lossy(&raw[..end]).trim().to_string();
        if text.is_empty() || text.starts_with('@') {
            None
        } else {
            Some(text)
        }
    }
}

#[cfg(not(windows))]
fn file_description(_path: &Path) -> Option<String> {
    None
}

fn application_name(path: &Path) -> String {
    let is_exe = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"));
    let friendly = if is_exe {
        file_description(path).or_else(|| shell_display_name(path))
    } else {
        shell_display_name(path).or_else(|| file_description(path))
    };

    match friendly {
        Some(name) if name.eq_ignore_ascii_case(&file_stem_name(path)) => file_stem_name(path),
        Some(name) => name,
        None => file_stem_name(path),
    }
}

#[tauri::command]
fn pick_applications() -> Result<Vec<ImportedApplication>, String> {
    let files = rfd::FileDialog::new()
        .set_title("选择要添加到 HORIZON 的应用")
        .add_filter(
            "应用程序与快捷方式",
            &["exe", "lnk", "bat", "cmd", "appref-ms", "url"],
        )
        .pick_files();

    let Some(files) = files else {
        return Ok(Vec::new());
    };

    let mut seen = std::collections::HashSet::new();
    let mut applications = Vec::new();
    for path in files {
        let key = path.to_string_lossy().to_lowercase();
        if !seen.insert(key) {
            continue;
        }
        applications.push(ImportedApplication {
            name: application_name(&path),
            path: path.to_string_lossy().into_owned(),
            icon: extract_icon(&path, 256),
        });
    }
    Ok(applications)
}

#[tauri::command]
fn pick_icon() -> Result<Option<String>, String> {
    let Some(path) = rfd::FileDialog::new()
        .set_title("选择应用图标")
        .add_filter("图片", &["png", "jpg", "jpeg", "webp", "ico", "bmp"])
        .pick_file()
    else {
        return Ok(None);
    };

    let image = image::open(&path).map_err(|error| format!("无法读取该图片：{error}"))?;
    let thumbnail = image.resize_to_fill(128, 128, image::imageops::FilterType::Lanczos3);
    let mut buffer = Cursor::new(Vec::new());
    thumbnail
        .write_to(&mut buffer, image::ImageFormat::Png)
        .map_err(|error| format!("无法转换图标：{error}"))?;
    Ok(Some(format!(
        "data:image/png;base64,{}",
        BASE64.encode(buffer.into_inner())
    )))
}

#[tauri::command]
fn launch_path(path: String, args: Option<String>) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("启动路径为空".into());
    }
    let args = args.unwrap_or_default();
    let args = if args.trim().is_empty() {
        None
    } else {
        Some(args.trim())
    };
    shell_open(&path, args).map_err(|error| format!("启动失败：{error}"))
}

#[derive(Clone, Copy)]
struct BuiltinStep {
    target: &'static str,
    args: Option<&'static str>,
    hint: Option<&'static str>,
}

const fn step(target: &'static str, args: Option<&'static str>) -> BuiltinStep {
    BuiltinStep {
        target,
        args,
        hint: None,
    }
}

#[cfg(windows)]
fn windows_build() -> u32 {
    use windows::core::{s, w};
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    type RtlGetVersion = unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32;
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // GetVersionExW reports the build the executable's manifest claims support
    // for (19041 with a Windows 10 manifest), which put this Windows 11 box on
    // the Windows 10 branch. RtlGetVersion is not manifest-aware.
    unsafe {
        let Ok(module) = GetModuleHandleW(w!("ntdll.dll")) else {
            return 0;
        };
        let Some(entry) = GetProcAddress(module, s!("RtlGetVersion")) else {
            return 0;
        };
        let rtl_get_version: RtlGetVersion = std::mem::transmute(entry);
        if rtl_get_version(&mut info) != 0 {
            return 0;
        }
    }
    info.dwBuildNumber
}

#[cfg(not(windows))]
fn windows_build() -> u32 {
    0
}

/// Which system tool backs 截屏 / 录屏 on this Windows version, in the order to
/// try them.
///
/// Windows 10 has no recorder inside Snipping Tool at all — that arrived in
/// Windows 11 — so a launch that only knows `SnippingTool.exe screenrecord`
/// does nothing there. 10's built-in recorder is Xbox Game Bar, and its
/// screenshot overlay is the `ms-screenclip:` URI served by Snip & Sketch.
fn builtin_steps(action: &str, build: u32) -> Option<Vec<BuiltinStep>> {
    let eleven = build >= 22_000;
    match action {
        "screenshot" if eleven => Some(vec![
            // Where the URI is still registered it opens the overlay directly;
            // current builds no longer carry it, so the tool's own window is the
            // fallback and the toast tells the user the hotkey.
            step("ms-screenclip:", None),
            BuiltinStep {
                target: "SnippingTool.exe",
                args: None,
                hint: Some("已打开截图工具，按 Win+Shift+S 开始截图"),
            },
        ]),
        "screenshot" => Some(vec![
            step("ms-screenclip:", None),
            step("SnippingTool.exe", None),
        ]),
        "screenrec" if eleven => Some(vec![BuiltinStep {
            target: "SnippingTool.exe",
            // The recorder has no command line switch — `screenrecord` is
            // accepted and ignored, and the window still opens in 截图 mode.
            args: None,
            hint: Some("已打开截图工具，点击「录制」或按 Win+Shift+R 开始录屏"),
        }]),
        "screenrec" => Some(vec![
            BuiltinStep {
                target: "ms-gamebar:",
                args: None,
                // Game Bar opens on its share widget; recording needs its hotkey.
                hint: Some("已打开 Xbox Game Bar，按 Win+Alt+R 开始录制"),
            },
            step("ms-gamingoverlay:", None),
        ]),
        _ => None,
    }
}

fn run_builtin_steps(steps: Vec<BuiltinStep>) -> Result<Option<String>, String> {
    let mut last_error = String::from("系统没有可用的相关应用");
    for step in steps {
        match shell_open(step.target, step.args) {
            Ok(()) => return Ok(step.hint.map(str::to_string)),
            Err(error) => last_error = format!("{}（{error}）", step.target),
        }
    }
    Err(format!("无法打开：{last_error}"))
}

#[tauri::command]
fn open_builtin(app: tauri::AppHandle, action: String) -> Result<Option<String>, String> {
    if let Some(steps) = builtin_steps(&action, windows_build()) {
        return run_builtin_steps(steps);
    }
    match action.as_str() {
        "projection" => shell_open("ms-settings:projections", None)
            .map_err(|error| format!("无法打开投影到此电脑设置：{error}"))?,
        "files" => {
            let documents = app
                .path()
                .document_dir()
                .map_err(|error| format!("无法定位文件目录：{error}"))?;
            shell_open(&documents.to_string_lossy(), None)
                .map_err(|error| format!("无法打开文件目录：{error}"))?;
        }
        "browser" => shell_open("https://www.bing.com", None)
            .map_err(|error| format!("无法打开浏览器：{error}"))?,
        "notepad" => shell_open("notepad.exe", None)
            .map_err(|error| format!("无法打开记事本：{error}"))?,
        other => return Err(format!("未知的内置应用：{other}")),
    }
    Ok(None)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MiracastStatus {
    /// `C:\Windows\System32\cast.exe` or `connect.exe` when the receiver is
    /// installed; `None` means Windows has nothing for this entry to launch.
    launcher: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MiracastLaunch {
    /// `"launched"` or `"settings"` — the frontend words its toast from this.
    mode: String,
}

/// Windows owns the Miracast receiver, so this app only exposes the entry:
/// launch the system app when it is installed, otherwise open 设置 > 系统 >
/// 投影到此电脑, the page that offers to add the 无线显示器 optional feature.
fn miracast_launcher() -> Option<String> {
    ["cast.exe", "connect.exe"]
        .into_iter()
        .map(|name| Path::new(r"C:\Windows\System32").join(name))
        .find(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
}

#[tauri::command]
fn miracast_status() -> Result<MiracastStatus, String> {
    Ok(MiracastStatus {
        launcher: miracast_launcher(),
    })
}

#[tauri::command]
fn miracast_launch() -> Result<MiracastLaunch, String> {
    match miracast_launcher() {
        Some(path) => {
            shell_open(&path, None).map_err(|error| format!("无法启动系统无线投屏：{error}"))?;
            Ok(MiracastLaunch {
                mode: "launched".into(),
            })
        }
        None => {
            shell_open("ms-settings:projections", None)
                .map_err(|error| format!("无法打开投影设置：{error}"))?;
            Ok(MiracastLaunch {
                mode: "settings".into(),
            })
        }
    }
}

#[tauri::command]
fn detect_office_apps() -> Vec<(String, String)> {
    #[cfg(windows)]
    {
        let mut available: Vec<(String, String)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        let mut add = |name: &str, path: &str| {
            // The same app often has shortcuts in several folders; keep the first.
            if seen.insert(name.trim().to_lowercase()) {
                available.push((name.to_string(), path.to_string()));
            }
        };

        let mut candidates: Vec<(&str, String)> = vec![
            (
                "Microsoft Word",
                r"C:\Program Files\Microsoft Office\root\Office16\WINWORD.EXE".to_string(),
            ),
            (
                "Microsoft Excel",
                r"C:\Program Files\Microsoft Office\root\Office16\EXCEL.EXE".to_string(),
            ),
            (
                "Microsoft PowerPoint",
                r"C:\Program Files\Microsoft Office\root\Office16\POWERPNT.EXE".to_string(),
            ),
            (
                "Microsoft Word",
                r"C:\Program Files (x86)\Microsoft Office\root\Office16\WINWORD.EXE".to_string(),
            ),
            (
                "Microsoft Excel",
                r"C:\Program Files (x86)\Microsoft Office\root\Office16\EXCEL.EXE".to_string(),
            ),
            (
                "Microsoft PowerPoint",
                r"C:\Program Files (x86)\Microsoft Office\root\Office16\POWERPNT.EXE".to_string(),
            ),
            (
                "Microsoft Word",
                r"C:\Program Files\Microsoft Office\root\Office15\WINWORD.EXE".to_string(),
            ),
            (
                "Microsoft Excel",
                r"C:\Program Files\Microsoft Office\root\Office15\EXCEL.EXE".to_string(),
            ),
            (
                "Microsoft PowerPoint",
                r"C:\Program Files\Microsoft Office\root\Office15\POWERPNT.EXE".to_string(),
            ),
            (
                "WPS Office",
                r"C:\Program Files\Kingsoft\WPS Office\ksolaunch.exe".to_string(),
            ),
            (
                "WPS Office",
                r"C:\Program Files (x86)\Kingsoft\WPS Office\ksolaunch.exe".to_string(),
            ),
            (
                "WPS 文字",
                r"C:\Program Files\Kingsoft\office6\wps.exe".to_string(),
            ),
            (
                "WPS 文字",
                r"C:\Program Files (x86)\Kingsoft\office6\wps.exe".to_string(),
            ),
            (
                "WPS 表格",
                r"C:\Program Files\Kingsoft\office6\et.exe".to_string(),
            ),
            (
                "WPS 演示",
                r"C:\Program Files\Kingsoft\office6\wpp.exe".to_string(),
            ),
            (
                "LibreOffice",
                r"C:\Program Files\LibreOffice\program\soffice.exe".to_string(),
            ),
            (
                "OnlyOffice",
                r"C:\Program Files\ONLYOFFICE\DesktopEditors\DesktopEditors.exe".to_string(),
            ),
        ];

        // WPS also ships per-user builds under %LOCALAPPDATA%.
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            candidates.push((
                "WPS Office",
                PathBuf::from(&local)
                    .join(r"Kingsoft\WPS Office\ksolaunch.exe")
                    .to_string_lossy()
                    .into_owned(),
            ));
            candidates.push((
                "WPS 文字",
                PathBuf::from(&local)
                    .join(r"Kingsoft\WPS Office\office6\wps.exe")
                    .to_string_lossy()
                    .into_owned(),
            ));
        }

        for (name, path) in &candidates {
            if Path::new(path).exists() {
                add(name, path);
            }
        }

        for dir in &shortcut_dirs() {
            collect_shortcuts(dir, 0, OFFICE_KEYWORDS, OFFICE_EXCLUDE, &mut add);
        }

        available
    }

    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[tauri::command]
fn detect_annotation_apps() -> Vec<(String, String)> {
    #[cfg(windows)]
    {
        let mut available: Vec<(String, String)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        let mut add = |name: &str, path: &str| {
            // The same app often has shortcuts in several folders; keep the first.
            if seen.insert(name.trim().to_lowercase()) {
                available.push((name.to_string(), path.to_string()));
            }
        };

        let candidates = [
            ("InKeys", r"C:\Program Files\InKeys\InKeys.exe"),
            ("InKeys", r"C:\Program Files (x86)\InKeys\InKeys.exe"),
            ("Ink Canvas", r"C:\Program Files\Ink Canvas\InkCanvas.exe"),
            ("Ink Canvas", r"C:\Program Files (x86)\Ink Canvas\InkCanvas.exe"),
            ("Drawboard PDF", r"C:\Program Files\Drawboard\DrawboardPDF.exe"),
            ("Epic Pen", r"C:\Program Files\Epic Pen\EpicPen.exe"),
            ("Epic Pen", r"C:\Program Files (x86)\Epic Pen\EpicPen.exe"),
            ("Pointofix", r"C:\Program Files\Pointofix\Pointofix.exe"),
            ("Pointofix", r"C:\Program Files (x86)\Pointofix\Pointofix.exe"),
            ("ZoomIt", r"C:\Program Files\Sysinternals\ZoomIt.exe"),
            ("ZoomIt", r"C:\Program Files\SysinternalsSuite\ZoomIt.exe"),
        ];

        for (name, path) in &candidates {
            if Path::new(path).exists() {
                add(name, path);
            }
        }

        // Store-installed Whiteboard lives under a versioned WindowsApps folder.
        let apps_root = Path::new(r"C:\Program Files\WindowsApps");
        if let Ok(entries) = std::fs::read_dir(apps_root) {
            for entry in entries.flatten() {
                let folder = entry.file_name().to_string_lossy().to_string();
                if folder.to_lowercase().starts_with("microsoft.whiteboard_") {
                    let exe = entry.path().join("Whiteboard.exe");
                    if exe.exists() {
                        add("Microsoft Whiteboard", &exe.to_string_lossy());
                        break;
                    }
                }
            }
        }

        // Start Menu shortcuts cover installers that use non-standard folders.
        for dir in &shortcut_dirs() {
            collect_shortcuts(dir, 0, ANNOTATION_KEYWORDS, &[], &mut add);
        }

        available
    }

    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[cfg(windows)]
fn shortcut_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from(
        r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs",
    )];
    if let Ok(appdata) = std::env::var("APPDATA") {
        dirs.push(PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs"));
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        dirs.push(PathBuf::from(&profile).join("Desktop"));
    }
    if let Ok(public) = std::env::var("PUBLIC") {
        dirs.push(PathBuf::from(public).join("Desktop"));
    }
    dirs
}

#[cfg(windows)]
const OFFICE_KEYWORDS: &[&str] = &[
    "word",
    "excel",
    "powerpoint",
    "powerpnt",
    "onenote",
    "wps",
    "libreoffice",
    "onlyoffice",
    "文档",
    "文字",
    "表格",
    "演示",
];

#[cfg(windows)]
const OFFICE_EXCLUDE: &[&str] = &[
    "wordpad",
    "写字板",
    "uninstall",
    "卸载",
    "readme",
    "更新",
    "update",
    "activation",
    "上载",
];

#[cfg(windows)]
const ANNOTATION_KEYWORDS: &[&str] = &[
    "inkeys",
    "ink canvas",
    "inkcanvas",
    "epic pen",
    "epicpen",
    "drawboard",
    "whiteboard",
    "pointofix",
    "zoomit",
    "批注",
    "annotation",
];

#[cfg(windows)]
fn collect_shortcuts<F: FnMut(&str, &str)>(
    dir: &Path,
    depth: u32,
    keywords: &[&str],
    exclude: &[&str],
    add: &mut F,
) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_shortcuts(&path, depth + 1, keywords, exclude, add);
            continue;
        }
        let is_shortcut = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"));
        if !is_shortcut {
            continue;
        }
        let stem = file_stem_name(&path);
        let lower = stem.to_lowercase();
        if exclude.iter().any(|word| lower.contains(word)) {
            continue;
        }
        if keywords.iter().any(|keyword| lower.contains(keyword)) {
            add(&stem, &path.to_string_lossy());
        }
    }
}

/// Reveals the unpacked Chrome/Edge extension so it can be loaded once per
/// browser profile. Packaged builds carry it as a resource; a dev checkout
/// keeps it beside `src-tauri`.
#[tauri::command]
fn open_extension_folder(app: tauri::AppHandle) -> Result<String, String> {
    let mut candidates = Vec::new();
    if let Ok(resources) = app.path().resource_dir() {
        candidates.push(resources.join("extension"));
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("extension"),
    );
    let folder = candidates
        .into_iter()
        .find(|path| path.join("manifest.json").is_file())
        .ok_or_else(|| "找不到浏览器扩展目录，请重新安装 HORIZON".to_string())?;
    shell_open(&folder.to_string_lossy(), None)
        .map_err(|error| format!("无法打开扩展目录：{error}"))?;
    Ok(folder.to_string_lossy().into_owned())
}

use std::sync::OnceLock;
static MEDIA_RUNTIME: OnceLock<Arc<media_stage::MediaRuntime>> = OnceLock::new();
static SNAPDROP_CONFIG: OnceLock<snapdrop_server::ConfigStore> = OnceLock::new();
static SNAPDROP_URL: OnceLock<String> = OnceLock::new();

/// The shared media runtime: one library, one stage, one player. Created on
/// first use so the receiver commands and the SnapDrop server see the same
/// objects — two decoders for one room would be two pictures fighting.
fn media_runtime(app: &tauri::AppHandle) -> Arc<media_stage::MediaRuntime> {
    Arc::clone(MEDIA_RUNTIME.get_or_init(|| {
        // Parent the video surface to the main window so DLNA/AirPlay pictures
        // sit inside the launcher instead of popping a top-level window.
        let video_parent = app
            .get_webview_window("main")
            .and_then(|w| w.hwnd().ok())
            .map(|hwnd| hwnd.0 as isize);
        Arc::new(media_stage::MediaRuntime::start(video_parent))
    }))
}

/// The SnapDrop page is a folder of static files, not something Vite bundles.
/// Installed builds carry it as a resource; dev builds read the source tree.
fn snapdrop_assets(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(dir) = app.path().resource_dir() {
        candidates.push(dir.join("snapdrop"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("snapdrop"));
            candidates.push(dir.join("assets").join("snapdrop"));
        }
    }
    candidates.push(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("snapdrop"),
    );
    candidates
        .into_iter()
        .find(|dir| dir.join("index.html").is_file())
        .ok_or_else(|| "找不到文件传送页面资源，请重新安装 HORIZON".to_string())
}

/// Async on purpose: the listener lives on the task that spawns it, so it has to
/// be awaited from a runtime that outlives this command. A private runtime
/// dropped after `block_on` closes the port again, which is why the panel's
/// address never opened from another machine.
#[tauri::command]
async fn start_snapdrop_server(app: tauri::AppHandle, port: u16) -> Result<String, String> {
    if let Some(url) = SNAPDROP_URL.get() {
        return Ok(url.clone());
    }
    let assets = snapdrop_assets(&app)?;
    let runtime = media_runtime(&app);
    let started = snapdrop_server::start_snapdrop_server(port, assets, app.clone(), runtime).await?;
    let _ = SNAPDROP_CONFIG.set(started.config);
    let _ = SNAPDROP_URL.set(started.url.clone());
    Ok(started.url)
}

#[tauri::command]
fn media_invoke(app: tauri::AppHandle, action: String, args: Option<serde_json::Value>) -> Result<serde_json::Value, String> {
    let runtime = media_runtime(&app);
    runtime.stage.invoke(&action, args.as_ref().unwrap_or(&serde_json::Value::Null))
}

fn snapdrop_config_store() -> Result<snapdrop_server::ConfigStore, String> {
    SNAPDROP_CONFIG
        .get()
        .cloned()
        .ok_or_else(|| "文件传送服务未启动".to_string())
}

#[tauri::command]
async fn set_snapdrop_device_name(name: String) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("设备名不能为空".into());
    }
    let store = snapdrop_config_store()?;
    let snapshot = {
        let mut config = store.write().await;
        config.device_name = trimmed.to_string();
        config.clone()
    };
    snapdrop_server::save_config(&snapshot)
}

#[tauri::command]
async fn set_snapdrop_storage_dir(path: String) -> Result<(), String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("存储目录不能为空".into());
    }
    // Fail here rather than on the first arriving file, when nobody is looking.
    std::fs::create_dir_all(trimmed).map_err(|error| format!("无法创建存储目录：{error}"))?;
    let store = snapdrop_config_store()?;
    let snapshot = {
        let mut config = store.write().await;
        config.storage_dir = trimmed.to_string();
        config.clone()
    };
    snapdrop_server::save_config(&snapshot)
}

#[tauri::command]
async fn get_snapdrop_config() -> Result<snapdrop_server::SnapdropConfig, String> {
    match SNAPDROP_CONFIG.get() {
        Some(store) => Ok(store.read().await.clone()),
        // The panel reads this before trusting the form; falling back to disk
        // keeps a saved name and folder visible even if the server is down.
        None => Ok(snapdrop_server::load_config()),
    }
}

#[tauri::command]
fn generate_snapdrop_qr() -> Result<String, String> {
    if let Some(url) = SNAPDROP_URL.get() {
        snapdrop_server::generate_qr_code(url)
    } else {
        Err("SnapDrop 服务未启动".to_string())
    }
}

#[tauri::command]
fn snapdrop_firewall_ok() -> bool {
    firewall::is_allowed()
}

/// Blocking on a UAC prompt, so it must not hold a runtime thread.
#[tauri::command]
async fn allow_snapdrop_firewall() -> Result<bool, String> {
    let code = tauri::async_runtime::spawn_blocking(firewall::allow)
        .await
        .map_err(|error| format!("放行失败：{error}"))??;
    if code != 0 {
        return Err(format!("Windows 拒绝了防火墙规则（netsh 退出码 {code}）"));
    }
    Ok(firewall::is_allowed())
}

#[tauri::command]
fn pick_snapdrop_storage_folder() -> Result<Option<String>, String> {
    use rfd::FileDialog;
    let folder = FileDialog::new()
        .set_title("选择文件存储目录")
        .pick_folder();
    Ok(folder.map(|path| path.to_string_lossy().to_string()))
}

#[tauri::command]
fn autostart_status() -> bool {
    #[cfg(windows)]
    {
        autostart::is_enabled()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[tauri::command]
fn set_autostart(enabled: bool) -> Result<bool, String> {
    #[cfg(windows)]
    {
        autostart::set_enabled(enabled)?;
        Ok(autostart::is_enabled())
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Err("当前平台暂不支持开机启动".into())
    }
}

#[tauri::command]
fn get_system_status() -> SystemStatus {
    read_system_status()
}

/// Fullscreen also has to beat the taskbar. A borderless window sized to the
/// monitor still loses to a taskbar that the shell keeps raised (always-on-top,
/// or simply not recognising the window as fullscreen), so the window is lifted
/// into the topmost band while fullscreen and dropped back out afterwards.
fn apply_fullscreen(window: &tauri::WebviewWindow, on: bool) -> Result<(), String> {
    if on {
        remember_frame(window);
        window.set_fullscreen(true).map_err(|error| error.to_string())?;
        FULLSCREEN_TOP.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Err(error) = window.set_always_on_top(true) {
            eprintln!("[WINDOW] 无法设置窗口置顶状态：{error}");
        }
    } else {
        FULLSCREEN_TOP.store(false, std::sync::atomic::Ordering::SeqCst);
        let _ = window.set_always_on_top(false);
        window.set_fullscreen(false).map_err(|error| error.to_string())?;
        restore_frame(window);
    }
    Ok(())
}

/// Leaving fullscreen leaves the window on the full-monitor rect instead of the
/// maximized frame the shell draws (`-8,-8,1880,1088` on a 1920×1080 monitor):
/// tao only un-hides the window, it does not re-apply the pre-fullscreen state.
/// The frame is captured on entry and re-applied on exit.
struct SavedFrame {
    maximized: bool,
    position: tauri::PhysicalPosition<i32>,
    size: tauri::PhysicalSize<u32>,
}

static SAVED_FRAME: std::sync::Mutex<Option<SavedFrame>> = std::sync::Mutex::new(None);

fn remember_frame(window: &tauri::WebviewWindow) {
    // Client size, not outer size: `set_size` writes the client area, so saving
    // the outer rect would grow the frame by the border on every round trip.
    let frame = match (window.is_maximized(), window.outer_position(), window.inner_size()) {
        (Ok(maximized), Ok(position), Ok(size)) => Some(SavedFrame {
            maximized,
            position,
            size,
        }),
        _ => None,
    };
    if let Ok(mut slot) = SAVED_FRAME.lock() {
        *slot = frame;
    }
}

fn restore_frame(window: &tauri::WebviewWindow) {
    let frame = SAVED_FRAME
        .lock()
        .ok()
        .and_then(|mut slot| slot.take());
    let Some(frame) = frame else { return };
    if frame.maximized {
        // SW_RESTORE then SW_MAXIMIZE is the only sequence that rewrites the
        // stale rect: maximising an already-maximized window is a no-op.
        let _ = window.unmaximize();
        if let Err(error) = window.maximize() {
            eprintln!("[WINDOW] 无法恢复最大化窗口：{error}");
        }
    } else {
        let _ = window.set_size(frame.size);
        let _ = window.set_position(frame.position);
    }
}

/// Set while we hold the topmost band for fullscreen, so a foreign exit (a
/// window manager shortcut, not our own toggle) can drop it again instead of
/// leaving the launcher pinned over every other window.
static FULLSCREEN_TOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[tauri::command]
fn window_control(window: tauri::WebviewWindow, action: String) -> Result<bool, String> {
    match action.as_str() {
        "minimize" => {
            window.minimize().map_err(|error| error.to_string())?;
            Ok(false)
        }
        "close" => {
            window.close().map_err(|error| error.to_string())?;
            Ok(false)
        }
        "toggleMaximize" => {
            let maximized = window.is_maximized().map_err(|error| error.to_string())?;
            if maximized {
                window.unmaximize().map_err(|error| error.to_string())?;
            } else {
                window.maximize().map_err(|error| error.to_string())?;
            }
            Ok(!maximized)
        }
        "toggleFullscreen" => {
            let fullscreen = window.is_fullscreen().map_err(|error| error.to_string())?;
            apply_fullscreen(&window, !fullscreen)?;
            Ok(!fullscreen)
        }
        "fullscreenOn" => {
            apply_fullscreen(&window, true)?;
            Ok(true)
        }
        "fullscreenOff" => {
            apply_fullscreen(&window, false)?;
            Ok(false)
        }
        "startDrag" => {
            window.start_dragging().map_err(|error| error.to_string())?;
            Ok(false)
        }
        other => Err(format!("未知的窗口操作：{other}")),
    }
}

#[tauri::command]
fn save_whiteboard(
    app: tauri::AppHandle,
    data_url: String,
    suggested_name: String,
) -> Result<Option<String>, String> {
    let encoded = data_url
        .split_once(',')
        .map(|(_, payload)| payload)
        .unwrap_or(data_url.as_str());
    let bytes = BASE64
        .decode(encoded)
        .map_err(|error| format!("白板图像数据无效：{error}"))?;

    let mut file_name = sanitize_file_name(&suggested_name);
    if !file_name.to_lowercase().ends_with(".png") {
        file_name.push_str(".png");
    }

    let mut dialog = rfd::FileDialog::new()
        .set_title("保存白板")
        .set_file_name(&file_name)
        .add_filter("PNG 图片", &["png"]);
    if let Ok(pictures) = app.path().picture_dir() {
        dialog = dialog.set_directory(pictures);
    }

    let Some(mut target) = dialog.save_file() else {
        return Ok(None);
    };
    if target.extension().is_none() {
        target.set_extension("png");
    }

    std::fs::write(&target, bytes).map_err(|error| format!("保存白板失败：{error}"))?;
    Ok(Some(target.to_string_lossy().into_owned()))
}

fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|character| match character {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            control if control.is_control() => '-',
            other => other,
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        "HORIZON-白板".into()
    } else {
        cleaned
    }
}

#[cfg(windows)]
fn extract_icon(path: &Path, size: u32) -> Option<String> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
    };

    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        if !initialized {
            return None;
        }

        let result = (|| -> Option<String> {
            let path_wide = HSTRING::from(path.to_string_lossy().as_ref());
            let factory: IShellItemImageFactory =
                SHCreateItemFromParsingName(PCWSTR(path_wide.as_ptr()), None).ok()?;
            let requested = size.clamp(32, 512) as i32;
            let bitmap = factory
                .GetImage(
                    SIZE {
                        cx: requested,
                        cy: requested,
                    },
                    SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
                )
                .ok()?;

            let mut info = BITMAP::default();
            let info_size = std::mem::size_of::<BITMAP>() as i32;
            let read = GetObjectW(
                HGDIOBJ(bitmap.0),
                info_size,
                Some(&mut info as *mut _ as *mut _),
            );
            if read == 0 || info.bmWidth <= 0 || info.bmHeight <= 0 {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                return None;
            }

            let width = info.bmWidth;
            let height = info.bmHeight;
            let mut bitmap_info = BITMAPINFO::default();
            bitmap_info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bitmap_info.bmiHeader.biWidth = width;
            bitmap_info.bmiHeader.biHeight = -height;
            bitmap_info.bmiHeader.biPlanes = 1;
            bitmap_info.bmiHeader.biBitCount = 32;
            bitmap_info.bmiHeader.biCompression = BI_RGB.0;

            let mut pixels = vec![0u8; width as usize * height as usize * 4];
            let device_context = GetDC(None);
            let copied = GetDIBits(
                device_context,
                bitmap,
                0,
                height as u32,
                Some(pixels.as_mut_ptr() as *mut _),
                &mut bitmap_info,
                DIB_RGB_COLORS,
            );
            let _ = ReleaseDC(None, device_context);
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            if copied == 0 {
                return None;
            }

            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }

            // Shell 返回的 HBITMAP 通常带预乘 Alpha，直接编码会出现黑边。
            let has_transparency = pixels.chunks_exact(4).any(|pixel| pixel[3] != 0);
            let premultiplied = has_transparency
                && pixels.chunks_exact(4).all(|pixel| {
                    let alpha = pixel[3] as u32;
                    alpha == 0
                        || (pixel[0] as u32 <= alpha + 2
                            && pixel[1] as u32 <= alpha + 2
                            && pixel[2] as u32 <= alpha + 2)
                });
            if premultiplied {
                for pixel in pixels.chunks_exact_mut(4) {
                    let alpha = pixel[3] as u32;
                    if alpha > 0 && alpha < 255 {
                        for channel in 0..3 {
                            pixel[channel] =
                                ((pixel[channel] as u32 * 255) / alpha).min(255) as u8;
                        }
                    }
                }
            }

            let image = image::RgbaImage::from_raw(width as u32, height as u32, pixels)?;
            let mut buffer = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image)
                .write_to(&mut buffer, image::ImageFormat::Png)
                .ok()?;
            Some(format!(
                "data:image/png;base64,{}",
                BASE64.encode(buffer.into_inner())
            ))
        })();

        CoUninitialize();
        result
    }
}

#[cfg(not(windows))]
fn extract_icon(_path: &Path, _size: u32) -> Option<String> {
    None
}

#[cfg(windows)]
fn shell_open(target: &str, args: Option<&str>) -> Result<(), String> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    unsafe {
        let operation = HSTRING::from("open");
        let file = HSTRING::from(target);
        let parameters = args.map(HSTRING::from);
        let parameter_ptr = parameters
            .as_ref()
            .map(|value| PCWSTR(value.as_ptr()))
            .unwrap_or(PCWSTR::null());
        let result = ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(file.as_ptr()),
            parameter_ptr,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        let code = result.0 as isize;
        if code <= 32 {
            let reason = match code {
                0 | 8 => "系统资源不足".to_string(),
                2 => "找不到该文件".to_string(),
                3 => "找不到该路径".to_string(),
                5 => "访问被拒绝".to_string(),
                31 => "没有可用的打开方式".to_string(),
                _ => format!("系统返回错误码 {code}"),
            };
            return Err(reason);
        }
        Ok(())
    }
}

#[cfg(not(windows))]
fn shell_open(_target: &str, _args: Option<&str>) -> Result<(), String> {
    Err("当前平台暂不支持此操作".into())
}

#[cfg(windows)]
fn read_system_status() -> SystemStatus {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    let mut status = SYSTEM_POWER_STATUS::default();
    if unsafe { GetSystemPowerStatus(&mut status) }.is_err() {
        return SystemStatus {
            battery_percent: None,
            charging: false,
        };
    }

    let has_battery = status.BatteryFlag & 0x80 == 0;
    let battery_percent = if has_battery && status.BatteryLifePercent != 255 {
        Some(status.BatteryLifePercent)
    } else {
        None
    };
    SystemStatus {
        battery_percent,
        charging: status.ACLineStatus == 1,
    }
}

#[cfg(not(windows))]
fn read_system_status() -> SystemStatus {
    SystemStatus {
        battery_percent: None,
        charging: false,
    }
}

#[tauri::command]
async fn create_sidebar_window(app: tauri::AppHandle) -> Result<(), String> {
    eprintln!("[SIDEBAR] create_sidebar_window command called");
    create_sidebar_window_sync(app)
}

fn create_sidebar_window_sync(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::WebviewWindowBuilder;
    
    eprintln!("[SIDEBAR] create_sidebar_window_sync called");
    
    // Check if sidebar window already exists
    if let Some(window) = app.get_webview_window("sidebar") {
        eprintln!("[SIDEBAR] Window already exists, showing and focusing");
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }
    
    eprintln!("[SIDEBAR] Creating new sidebar window with minimal options");
    
    // Retry logic for WebView2 environment issues (error 0x8007139F)
    let mut last_error = String::new();
    for attempt in 1..=3 {
        let result = WebviewWindowBuilder::new(
            &app,
            "sidebar",
            tauri::WebviewUrl::App("/sidebar.html".into())
        )
        .title("HORIZON Sidebar")
        .inner_size(80.0, 600.0)
        .position(100.0, 100.0)
        .build();
        
        match result {
            Ok(window) => {
                eprintln!("[SIDEBAR] Window created successfully on attempt {}, label: {}", attempt, window.label());
                // Apply additional settings after creation
                let _ = window.set_resizable(false);
                let _ = window.set_decorations(false);
                let _ = window.set_always_on_top(true);
                let _ = window.set_skip_taskbar(true);
                return Ok(());
            }
            Err(e) => {
                last_error = e.to_string();
                eprintln!("[SIDEBAR] Attempt {} failed: {}", attempt, last_error);
                if attempt < 3 {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        }
    }
    
    Err(format!("Failed to create sidebar window after 3 attempts: {}", last_error))
}

#[tauri::command]
async fn create_annotation_window(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::WebviewWindowBuilder;
    
    eprintln!("[ANNOTATION] create_annotation_window called");
    
    // Check if annotation window already exists
    if let Some(window) = app.get_webview_window("annotation") {
        eprintln!("[ANNOTATION] Window already exists, showing and focusing");
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }
    
    // Get screen size for fullscreen overlay
    let monitor = app.primary_monitor().map_err(|e| e.to_string())?;
    let (width, height) = if let Some(monitor) = monitor {
        let size = monitor.size();
        (size.width as f64, size.height as f64)
    } else {
        (1920.0, 1080.0)
    };
    
    eprintln!("[ANNOTATION] Creating new annotation window ({}x{})", width, height);
    
    // Retry logic for WebView2 environment issues (error 0x8007139F)
    let mut last_error = String::new();
    for attempt in 1..=3 {
        match WebviewWindowBuilder::new(
            &app,
            "annotation",
            tauri::WebviewUrl::App("/annotation.html".into())
        )
        .title("HORIZON Annotation")
        .inner_size(width, height)
        .position(0.0, 0.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(false)
        .build()
        {
            Ok(window) => {
                eprintln!("[ANNOTATION] Window created successfully on attempt {}, label: {}", attempt, window.label());
                return Ok(());
            }
            Err(e) => {
                last_error = e.to_string();
                eprintln!("[ANNOTATION] Attempt {} failed: {}", attempt, last_error);
                if attempt < 3 {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
    }
    
    Err(format!("Failed to create annotation window after 3 attempts: {}", last_error))
}

#[tauri::command]
async fn close_sidebar_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("sidebar") {
        window.close().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
async fn close_annotation_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("annotation") {
        window.close().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Two launchers means two WebView2 environments and, in practice, one of
    // them failing to create with 0x8007139F. Keep exactly one alive and bring
    // it forward when the shortcut is pressed again.
    #[cfg(windows)]
    let _instance = match single_instance() {
        Some(handle) => handle,
        None => return,
    };

    let app = tauri::Builder::default()
        .manage(Receivers::default())
        .manage(Sender::default())
        .invoke_handler(tauri::generate_handler![
            load_state,
            save_state,
            load_welcome_media,
            save_welcome_media,
            pick_applications,
            pick_icon,
            launch_path,
            open_builtin,
            miracast_status,
            miracast_launch,
            open_extension_folder,
            media_invoke,
            start_snapdrop_server,
            set_snapdrop_device_name,
            set_snapdrop_storage_dir,
            get_snapdrop_config,
            generate_snapdrop_qr,
            pick_snapdrop_storage_folder,
            snapdrop_firewall_ok,
            allow_snapdrop_firewall,
            detect_office_apps,
            detect_annotation_apps,
            get_system_status,
            window_control,
            autostart_status,
            set_autostart,
            create_sidebar_window,
            create_annotation_window,
            close_sidebar_window,
            close_annotation_window,
            save_whiteboard,
            receivers::airplay_start,
            receivers::airplay_stop,
            receivers::airplay_preview,
            receivers::airplay_preview_stop,
            receivers::mirror_perf_report,
            receivers::dlna_start,
            receivers::dlna_stop,
            receivers::browser_start,
            receivers::browser_stop,
            receivers::browser_preview,
            receivers::browser_preview_stop,
            receivers::browser_keyframe,
            receivers::receiver_status,
            receivers::discover_peers,
            sender::cast_send,
            sender::cast_control,
            sender::cast_status
        ])
        .build(tauri::generate_context!())
        .expect("error while running HORIZON Meeting");

    app.run(|handle, event| {
        if let tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::Resized(_),
            ..
        } = &event
        {
            if label == "main" && FULLSCREEN_TOP.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(window) = handle.get_webview_window("main") {
                    if !window.is_fullscreen().unwrap_or(false) {
                        FULLSCREEN_TOP.store(false, std::sync::atomic::Ordering::SeqCst);
                        let _ = window.set_always_on_top(false);
                    }
                }
            }
        }
        if let tauri::RunEvent::Exit = event {
            if let Some(receivers) = handle.try_state::<Receivers>() {
                let _ = receivers.stop_airplay();
                let _ = receivers.stop_dlna();
                let _ = receivers.stop_browser();
            }
            if let Some(sender) = handle.try_state::<Sender>() {
                sender.shutdown();
            }
        }
    });
}

/// Returns the mutex handle to hold for the process lifetime, or `None` when
/// another instance already owns it (in which case that window is focused).
#[cfg(windows)]
fn single_instance() -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::core::w;
    use windows::Win32::Foundation::{GetLastError, SetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    unsafe {
        SetLastError(windows::Win32::Foundation::ERROR_SUCCESS);
        let mutex = CreateMutexW(None, false, w!("Local\\HORIZON.MeetingHub")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(window) = FindWindowW(None, w!("HORIZON 会议中枢")) {
                let _ = ShowWindow(window, SW_RESTORE);
                let _ = SetForegroundWindow(window);
            }
            return None;
        }
        Some(mutex)
    }
}

#[cfg(not(windows))]
fn single_instance() -> Option<()> {
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(action: &str, build: u32) -> Vec<&'static str> {
        builtin_steps(action, build)
            .expect("action has a plan")
            .iter()
            .map(|step| step.target)
            .collect()
    }

    #[test]
    fn windows_11_screenshot_prefers_the_overlay_uri_then_the_tool() {
        assert_eq!(
            targets("screenshot", 22_631),
            vec!["ms-screenclip:", "SnippingTool.exe"]
        );
        assert_eq!(
            targets("screenshot", 22_000),
            vec!["ms-screenclip:", "SnippingTool.exe"]
        );
    }

    #[test]
    fn windows_10_screenshot_goes_through_the_snip_overlay() {
        assert_eq!(
            targets("screenshot", 19_045),
            vec!["ms-screenclip:", "SnippingTool.exe"]
        );
    }

    #[test]
    fn windows_11_recording_opens_the_tool_and_names_its_hotkey() {
        let steps = builtin_steps("screenrec", 22_631).unwrap();
        assert_eq!(steps[0].target, "SnippingTool.exe");
        // `screenrecord` is silently ignored by the tool, so it must not be
        // passed on as if it selected the recorder.
        assert_eq!(steps[0].args, None);
        assert!(steps[0].hint.unwrap().contains("Win+Shift+R"));
    }

    #[test]
    fn windows_10_never_reaches_for_a_recorder_snipping_tool_lacks() {
        let steps = builtin_steps("screenrec", 19_045).unwrap();
        assert_eq!(steps[0].target, "ms-gamebar:");
        assert!(steps[0].hint.is_some(), "Game Bar needs a hotkey hint");
        assert!(steps
            .iter()
            .all(|step| step.target != "SnippingTool.exe"));
    }

    #[test]
    fn other_actions_keep_their_own_route() {
        assert!(builtin_steps("notepad", 22_631).is_none());
        assert!(builtin_steps("files", 19_045).is_none());
    }

    #[test]
    fn the_build_probe_reports_something() {
        // A zero would silently route Windows 11 through the Windows 10 plan.
        assert!(windows_build() >= 10_240, "build number probe failed");
    }
}
