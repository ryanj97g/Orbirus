// Orbirus — a Fences-style desktop organizer in native Rust + Win32.
// Release builds suppress the console (M7); debug builds keep it so
// println! diagnostics stay visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod desktop;
mod fence;
mod icons;
mod launch;
mod render;
mod rules;
mod rules_ui;
mod settings_ui;
mod shellmenu;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::{InitCommonControlsEx, ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// WM_APP + 1 is reserved for desktop-change notifications (M7 file watcher).
const WM_TRAYICON: u32 = WM_APP + 2;
// Posted by the mouse hook when a click lands outside every fence.
const WM_OUTSIDE_CLICK: u32 = WM_APP + 3;
const IDM_EXIT: usize = 1;
const IDM_NEW_FENCE: usize = 2;
const IDM_SETTINGS: usize = 3;
const IDM_SORT_UNSORTED: usize = 4;
const IDM_AUTOSTART: usize = 5;
const IDM_UNDO_SORT: usize = 6;
const IDM_SAVE_LAYOUT: usize = 7;
const IDM_RESTORE_BASE: usize = 700; // ..709, indexes SNAPSHOT_LIST
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

// The hook callback needs the main window across threads-agnostic static.
static MAIN_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

thread_local! {
    // M12: (item, from_fence_id, to_fence_id) of the last "Sort Unsorted
    // now" run, for Undo last sort.
    static LAST_SORT: std::cell::RefCell<Vec<(String, String, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    // M12: snapshot files backing the Restore layout submenu, newest first.
    static SNAPSHOT_LIST: std::cell::RefCell<Vec<std::path::PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}
const TRAY_UID: u32 = 1;
// Debounces watcher bursts (Explorer fires several events per operation).
const TIMER_REFRESH: usize = 2;

fn main() -> Result<()> {
    unsafe {
        // v1.2: launched from Explorer's New menu? Clean up any placeholder
        // file Explorer named for us, and forward to a running instance.
        let args: Vec<String> = std::env::args().collect();
        let shellnew = args.iter().any(|a| a.eq_ignore_ascii_case("/shellnew"));
        if shellnew {
            for a in &args {
                if a.to_ascii_lowercase().ends_with(".orbirusfence")
                    && std::path::Path::new(a).exists()
                {
                    let _ = std::fs::remove_file(a);
                }
            }
        }

        // M10: single instance — a second launch would fight the first over
        // config.json (last writer wins), so it exits immediately (after
        // forwarding a /shellnew request).
        let _instance_mutex = CreateMutexW(None, true, w!("Local\\OrbirusInstance"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if shellnew {
                let main = FindWindowW(w!("OrbirusMain"), w!("Orbirus"));
                if let Ok(main) = main {
                    let _ = PostMessageW(
                        main,
                        WM_COMMAND,
                        WPARAM(IDM_NEW_FENCE),
                        LPARAM(0),
                    );
                }
            } else {
                println!("Orbirus is already running.");
            }
            return Ok(());
        }

        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?;
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        // ICC_WIN95_CLASSES registers the tooltip class (among others).
        let _ = InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_WIN95_CLASSES,
        });

        let hinstance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class_name = w!("OrbirusMain");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class_name,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(Error::from_win32());
        }

        // Hidden window: receives tray callbacks and owns app lifetime.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("Orbirus"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            hinstance,
            None,
        )?;

        add_tray_icon(hwnd)?;

        // M3: fences come from config; first run creates the default
        // ("Unsorted") config and shows the §2 setup instruction once.
        let load_result = config::load();
        let first_run = matches!(load_result, config::LoadResult::Missing);
        let cfg = match load_result {
            config::LoadResult::Loaded(c) => c,
            // M9: a truly absent config gets the five starter fences; the
            // desktop::refresh below then distributes every item through
            // their rules. A corrupt config must never re-fence — plain
            // default (single Unsorted) instead.
            config::LoadResult::Missing => first_run_config(),
            config::LoadResult::Corrupt => config::Config::default(),
        };
        config::init(cfg, hwnd);

        // M7: fences that would restore entirely offscreen (monitor removed
        // or rearranged) get pulled back onto the primary monitor, staggered.
        let rescued = config::with(|cfg| {
            let mut changed = false;
            for (i, f) in cfg.fences.iter_mut().enumerate() {
                let rc = RECT {
                    left: f.x,
                    top: f.y,
                    right: f.x + f.w,
                    bottom: f.y + f.h,
                };
                if MonitorFromRect(&rc, MONITOR_DEFAULTTONULL).is_invalid() {
                    f.x = 100 + i as i32 * 40;
                    f.y = 80 + i as i32 * 40;
                    changed = true;
                }
            }
            changed
        });

        // M4: everything on the real Desktop folders appears exactly once;
        // unassigned items land in "Unsorted".
        let desktop_items = desktop::enumerate();
        let (changed, _) = desktop::refresh(&desktop_items);
        if first_run || changed || rescued {
            config::save_now();
        }
        println!("Desktop items: {}", desktop_items.len());

        // Extract all icons up front (never during paint), at the configured
        // size scaled to this monitor's DPI.
        let all_items: Vec<String> =
            config::with(|c| c.fences.iter().flat_map(|f| f.items.iter().cloned()).collect());
        let icon_px = config::with(|c| c.icon_size) * GetDpiForWindow(hwnd) / 96;
        icons::preload(&all_items, icon_px);
        println!("Cached icons for {} item(s).", all_items.len());

        fence::register_class(hinstance)?;
        rules_ui::register_class(hinstance)?;
        settings_ui::register_class(hinstance)?;
        let fence_cfgs = config::with(|c| c.fences.clone());
        for fc in &fence_cfgs {
            fence::create_fence(hinstance, fc)?;
        }
        println!("Loaded {} fence(s) from config.", fence_cfgs.len());

        // M7: live updates from the real Desktop folders.
        desktop::start_watcher(hwnd);

        // Clicks outside any fence clear icon selections (user request).
        MAIN_HWND.store(hwnd.0 as isize, std::sync::atomic::Ordering::Relaxed);
        let _ = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0);

        // v1.2: Explorer's desktop New menu gets an "Orbirus Fence" entry.
        register_shell_new();
        if shellnew {
            create_new_fence();
        }

        if first_run {
            MessageBoxW(
                hwnd,
                w!("Orbirus shows your desktop items inside fences. To avoid seeing everything twice, hide Windows' own desktop icons:\n\nRight-click the desktop \u{2192} View \u{2192} uncheck \"Show desktop icons\"."),
                w!("Orbirus"),
                MB_OK | MB_ICONINFORMATION,
            );
        }

        println!("Orbirus running. (This console is for development only — you don't need to do anything here.)");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            // Give our dialogs standard Enter/Esc/Tab handling.
            let handled = [
                fence::rename_dialog_hwnd(),
                rules_ui::dialog_hwnd(),
                settings_ui::dialog_hwnd(),
            ]
            .into_iter()
            .any(|d| !d.0.is_null() && IsDialogMessageW(d, &msg).as_bool());
            if handled {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        CoUninitialize();
        Ok(())
    }
}

/// Desktop folder changed: re-enumerate, diff into config, cache icons for
/// arrivals, give any fence created by the diff (a fresh "Unsorted") a
/// window, and repaint.
unsafe fn on_desktop_changed(hwnd: HWND) {
    let items = desktop::enumerate();
    let (changed, added) = desktop::refresh(&items);
    if !changed {
        return;
    }
    config::schedule_save();
    if !added.is_empty() {
        let icon_px = config::with(|c| c.icon_size) * GetDpiForWindow(hwnd) / 96;
        icons::preload(&added, icon_px);
    }
    let missing: Vec<config::FenceConfig> = config::with(|c| {
        c.fences
            .iter()
            .filter(|f| fence::hwnd_for(&f.id).is_none())
            .cloned()
            .collect()
    });
    if let Ok(hmodule) = GetModuleHandleW(None) {
        for fc in &missing {
            let _ = fence::create_fence(hmodule.into(), fc);
        }
    }
    fence::invalidate_all();
}

/// v1.2: registers the ShellNew hooks (HKCU, no admin) so Explorer's
/// right-click New submenu offers "Orbirus Fence", which runs us with
/// /shellnew. Idempotent; refreshes the exe path each launch.
unsafe fn register_shell_new() {
    let mut exe = [0u16; 1024];
    let len = GetModuleFileNameW(None, &mut exe) as usize;
    if len == 0 {
        return;
    }
    let exe = String::from_utf16_lossy(&exe[..len]);
    let set = |subkey: &str, value: Option<&str>, data: &str| {
        let mut sk: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
        let vn: Option<Vec<u16>> =
            value.map(|v| v.encode_utf16().chain(std::iter::once(0)).collect());
        let d: Vec<u16> = data.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(sk.as_mut_ptr()),
            vn.as_ref()
                .map(|v| PCWSTR(v.as_ptr()))
                .unwrap_or(PCWSTR::null()),
            REG_SZ.0,
            Some(d.as_ptr() as *const std::ffi::c_void),
            (d.len() * 2) as u32,
        );
    };
    set(
        "Software\\Classes\\.orbirusfence",
        None,
        "Orbirus.Fence",
    );
    set("Software\\Classes\\Orbirus.Fence", None, "Orbirus Fence");
    set(
        "Software\\Classes\\Orbirus.Fence\\DefaultIcon",
        None,
        &format!("\"{exe}\",0"),
    );
    set(
        "Software\\Classes\\.orbirusfence\\ShellNew",
        Some("Command"),
        &format!("\"{exe}\" /shellnew \"%1\""),
    );
}

/// Low-level mouse hook: a left-button press over anything that isn't a
/// fence posts WM_OUTSIDE_CLICK so selections clear. Kept minimal — real
/// work happens on the main window's message loop.
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && wparam.0 as u32 == WM_LBUTTONDOWN {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let over = GetAncestor(WindowFromPoint(info.pt), GA_ROOT);
        let mut cls = [0u16; 32];
        let len = GetClassNameW(over, &mut cls) as usize;
        if String::from_utf16_lossy(&cls[..len]) != "OrbirusFence" {
            let main = MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed);
            if main != 0 {
                let _ = PostMessageW(
                    HWND(main as *mut _),
                    WM_OUTSIDE_CLICK,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// M10: "Start with Windows" via the HKCU Run key.
pub(crate) unsafe fn autostart_enabled() -> bool {
    RegGetValueW(
        HKEY_CURRENT_USER,
        RUN_KEY,
        w!("Orbirus"),
        RRF_RT_REG_SZ,
        None,
        None,
        None,
    ) == ERROR_SUCCESS
}

pub(crate) unsafe fn set_autostart(enable: bool) {
    if enable {
        let mut path = [0u16; 1024];
        let len = GetModuleFileNameW(None, &mut path) as usize;
        if len == 0 {
            return;
        }
        // Quoted path, the Run-key convention for paths with spaces.
        let mut value: Vec<u16> = Vec::with_capacity(len + 3);
        value.push('"' as u16);
        value.extend_from_slice(&path[..len]);
        value.push('"' as u16);
        value.push(0);
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            w!("Orbirus"),
            REG_SZ.0,
            Some(value.as_ptr() as *const std::ffi::c_void),
            (value.len() * 2) as u32,
        );
    } else {
        let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("Orbirus"));
    }
}

/// M9 first-run config: five starter fences laid out left-to-right from the
/// top-left of the primary monitor, 420x300 with ~24px gaps, wrapping to a
/// second row if they don't fit. Apps/Documents/Pictures/Folders each carry
/// their category rule; Unsorted is the rule-less catch-all ("media" has no
/// starter fence, so videos/music land there).
unsafe fn first_run_config() -> config::Config {
    let hmon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let _ = GetMonitorInfoW(hmon, &mut mi);

    const STARTERS: [(&str, Option<&str>); 5] = [
        ("Apps", Some("apps")),
        ("Documents", Some("documents")),
        ("Pictures", Some("pictures")),
        ("Folders", Some("folders")),
        ("Unsorted", None),
    ];
    let (w, h, gap) = (420, 300, 24);
    let (x0, y0) = (mi.rcWork.left + gap, mi.rcWork.top + gap);
    let (mut x, mut y) = (x0, y0);
    let mut fences = Vec::new();
    for (i, (title, category)) in STARTERS.iter().enumerate() {
        if x + w > mi.rcWork.right {
            x = x0;
            y += h + gap;
        }
        fences.push(config::FenceConfig {
            id: format!("fence-{}", i + 1),
            title: title.to_string(),
            x,
            y,
            w,
            h,
            monitor: 0,
            rolled_up: false,
            color: "#1E1E2E".to_string(),
            opacity: 0.78,
            corner_radius: 12.0,
            items: Vec::new(),
            rules: category
                .map(|c| {
                    vec![rules::Rule {
                        kind: rules::RuleKind::Category,
                        value: c.to_string(),
                    }]
                })
                .unwrap_or_default(),
        });
        x += w + gap;
    }
    config::Config {
        version: 2,
        icon_size: 48,
        fences,
    }
}

/// Tray "Sort Unsorted now" (M8): run all rules against everything in
/// Unsorted; confirm before moving (M12: the confirmation lists where
/// things go, and the run is undoable). Manual placements elsewhere are
/// never touched.
unsafe fn sort_unsorted_now(hwnd: HWND) {
    let (moves, uid) = config::with(|cfg| {
        let Some(u) = cfg.fences.iter().position(|f| f.title == "Unsorted") else {
            return (Vec::new(), String::new());
        };
        let uid = cfg.fences[u].id.clone();
        let items = cfg.fences[u].items.clone();
        let cfg_ref = &*cfg;
        let moves: Vec<(String, String)> = items
            .into_iter()
            .filter_map(|item| {
                rules::match_fence(std::path::Path::new(&item), cfg_ref)
                    .filter(|target| *target != uid)
                    .map(|target| (item, target))
            })
            .collect();
        (moves, uid)
    });
    if moves.is_empty() {
        MessageBoxW(
            hwnd,
            w!("Nothing in Unsorted matches any rule."),
            w!("Orbirus"),
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    }
    // M12 preview: per-destination counts, in fence order of first mention.
    let mut dest_counts: Vec<(String, usize)> = Vec::new();
    for (_, target) in &moves {
        let title = config::with(|c| {
            c.fences
                .iter()
                .find(|f| f.id == *target)
                .map(|f| f.title.clone())
                .unwrap_or_default()
        });
        match dest_counts.iter_mut().find(|(t, _)| *t == title) {
            Some((_, n)) => *n += 1,
            None => dest_counts.push((title, 1)),
        }
    }
    let mut msg = format!("This will move {} items:\n", moves.len());
    for (title, n) in &dest_counts {
        msg.push_str(&format!("\n{n} \u{2192} {title}"));
    }
    msg.push_str("\n\nContinue?");
    let text: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    let answer = MessageBoxW(hwnd, PCWSTR(text.as_ptr()), w!("Orbirus"), MB_YESNO | MB_ICONQUESTION);
    if answer != IDYES {
        return;
    }
    let mut applied: Vec<(String, String, String)> = Vec::new();
    config::with(|cfg| {
        for (item, target_id) in &moves {
            let Some(u) = cfg.fences.iter().position(|f| f.title == "Unsorted") else {
                return;
            };
            if let Some(pos) = cfg.fences[u].items.iter().position(|p| p == item) {
                let it = cfg.fences[u].items.remove(pos);
                if let Some(t) = cfg.fences.iter_mut().find(|f| f.id == *target_id) {
                    t.items.push(it);
                    applied.push((item.clone(), uid.clone(), target_id.clone()));
                }
            }
        }
    });
    LAST_SORT.with(|l| *l.borrow_mut() = applied);
    config::schedule_save();
    fence::invalidate_all();
}

/// M12: reverses the last "Sort Unsorted now" run (items still where the
/// sort put them get moved back).
unsafe fn undo_last_sort() {
    let moves = LAST_SORT.with(|l| std::mem::take(&mut *l.borrow_mut()));
    if moves.is_empty() {
        return;
    }
    config::with(|cfg| {
        for (item, from_id, to_id) in &moves {
            let Some(t) = cfg.fences.iter_mut().find(|f| f.id == *to_id) else {
                continue;
            };
            let Some(pos) = t.items.iter().position(|p| p == item) else {
                continue;
            };
            let it = t.items.remove(pos);
            if let Some(f) = cfg.fences.iter_mut().find(|f| f.id == *from_id) {
                f.items.push(it);
            }
        }
    });
    config::schedule_save();
    fence::invalidate_all();
}

/// M12: copies the current config to a timestamped snapshot file.
unsafe fn save_layout_snapshot() {
    config::save_now();
    let dir = config::snapshots_dir();
    let _ = std::fs::create_dir_all(&dir);
    let st = windows::Win32::System::SystemInformation::GetLocalTime();
    let name = format!(
        "layout-{:04}{:02}{:02}-{:02}{:02}{:02}.json",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    );
    let _ = std::fs::copy(config::path(), dir.join(name));
}

/// Newest snapshots first, capped at the 10 the menu can hold.
fn list_snapshots() -> Vec<std::path::PathBuf> {
    let mut snaps: Vec<std::path::PathBuf> = std::fs::read_dir(config::snapshots_dir())
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    snaps.sort();
    snaps.reverse();
    snaps.truncate(10);
    snaps
}

/// "layout-20260702-153045" -> "2026-07-02 15:30:45"
fn snapshot_label(path: &std::path::Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
    let digits: String = stem.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() == 14 {
        format!(
            "{}-{}-{} {}:{}:{}",
            &digits[0..4],
            &digits[4..6],
            &digits[6..8],
            &digits[8..10],
            &digits[10..12],
            &digits[12..14]
        )
    } else {
        stem.to_string()
    }
}

/// M12: replaces the live layout with a snapshot — tears down all fence
/// windows, swaps the config, reconciles against today's desktop, rebuilds.
unsafe fn restore_layout(hwnd: HWND, snap: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(snap) else {
        return;
    };
    let text = text.trim_start_matches('\u{feff}');
    let Ok(mut cfg) = serde_json::from_str::<config::Config>(text) else {
        MessageBoxW(
            hwnd,
            w!("That snapshot can't be read."),
            w!("Orbirus"),
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    };
    cfg.version = 2;
    for f in &mut cfg.fences {
        f.items.retain(|p| std::path::Path::new(p).exists());
    }
    fence::destroy_all();
    config::replace(cfg);
    let items = desktop::enumerate();
    let _ = desktop::refresh(&items);
    config::save_now();
    let all: Vec<String> =
        config::with(|c| c.fences.iter().flat_map(|f| f.items.iter().cloned()).collect());
    let icon_px = config::with(|c| c.icon_size) * GetDpiForWindow(hwnd) / 96;
    icons::preload(&all, icon_px);
    if let Ok(hmodule) = GetModuleHandleW(None) {
        for fc in config::with(|c| c.fences.clone()) {
            let _ = fence::create_fence(hmodule.into(), &fc);
        }
    }
    LAST_SORT.with(|l| l.borrow_mut().clear());
}

/// Tray "New Fence": a 300x200 fence centered on the cursor's monitor.
unsafe fn create_new_fence() {
    let Ok(hmodule) = GetModuleHandleW(None) else {
        return;
    };
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let _ = GetMonitorInfoW(hmon, &mut mi);
    let fc = config::FenceConfig {
        id: config::with(|c| config::next_id_for(c)),
        title: "New Fence".to_string(),
        x: (mi.rcWork.left + mi.rcWork.right) / 2 - 150,
        y: (mi.rcWork.top + mi.rcWork.bottom) / 2 - 100,
        w: 300,
        h: 200,
        monitor: 0,
        rolled_up: false,
        color: "#1E1E2E".to_string(),
        opacity: 0.78,
        corner_radius: 12.0,
        items: Vec::new(),
        rules: Vec::new(),
    };
    config::with(|c| c.fences.push(fc.clone()));
    config::schedule_save();
    if let Ok(hwnd) = fence::create_fence(hmodule.into(), &fc) {
        // M10: name it right away instead of leaving a "New Fence" around.
        fence::begin_rename(hwnd);
    }
}

unsafe fn add_tray_icon(hwnd: HWND) -> Result<()> {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAYICON,
        // The embedded app icon (resource id 1 from build.rs), falling back
        // to the stock icon if the resource is missing.
        hIcon: LoadIconW(GetModuleHandleW(None)?, PCWSTR(1 as *const u16))
            .or_else(|_| LoadIconW(None, IDI_APPLICATION))?,
        ..Default::default()
    };
    for (i, c) in "Orbirus".encode_utf16().enumerate() {
        nid.szTip[i] = c;
    }
    if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
        return Err(Error::from_win32());
    }
    Ok(())
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let _ = AppendMenuW(menu, MF_STRING, IDM_NEW_FENCE, w!("New Fence"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_SORT_UNSORTED, w!("Sort Unsorted now"));
    let undo_flags = if LAST_SORT.with(|l| l.borrow().is_empty()) {
        MF_STRING | MF_GRAYED
    } else {
        MF_STRING
    };
    let _ = AppendMenuW(menu, undo_flags, IDM_UNDO_SORT, w!("Undo last sort"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_SAVE_LAYOUT, w!("Save layout"));
    // Restore layout submenu: newest snapshots first, up to 10.
    let snaps = list_snapshots();
    if let Ok(restore_menu) = CreatePopupMenu() {
        if snaps.is_empty() {
            let _ = AppendMenuW(restore_menu, MF_STRING | MF_GRAYED, 0, w!("(no snapshots)"));
        } else {
            for (i, snap) in snaps.iter().enumerate() {
                let label = snapshot_label(snap);
                let mut wide: Vec<u16> = label.encode_utf16().collect();
                wide.push(0);
                let _ = AppendMenuW(
                    restore_menu,
                    MF_STRING,
                    IDM_RESTORE_BASE + i,
                    PCWSTR(wide.as_ptr()),
                );
            }
        }
        let _ = AppendMenuW(menu, MF_POPUP, restore_menu.0 as usize, w!("Restore layout"));
    }
    SNAPSHOT_LIST.with(|l| *l.borrow_mut() = snaps);
    let autostart_flags = if autostart_enabled() {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING
    };
    let _ = AppendMenuW(menu, autostart_flags, IDM_AUTOSTART, w!("Start with Windows"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_SETTINGS, w!("Settings…"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT, w!("Exit"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Required so the menu dismisses when clicking elsewhere.
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAYICON => {
                let mouse_msg = lparam.0 as u32;
                if mouse_msg == WM_RBUTTONUP || mouse_msg == WM_CONTEXTMENU {
                    show_tray_menu(hwnd);
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match wparam.0 & 0xFFFF {
                    IDM_EXIT => {
                        let _ = DestroyWindow(hwnd);
                    }
                    IDM_NEW_FENCE => create_new_fence(),
                    IDM_SORT_UNSORTED => sort_unsorted_now(hwnd),
                    IDM_UNDO_SORT => undo_last_sort(),
                    IDM_SAVE_LAYOUT => save_layout_snapshot(),
                    IDM_AUTOSTART => set_autostart(!autostart_enabled()),
                    IDM_SETTINGS => settings_ui::open(hwnd),
                    c if (IDM_RESTORE_BASE..IDM_RESTORE_BASE + 10).contains(&c) => {
                        let snap = SNAPSHOT_LIST
                            .with(|l| l.borrow().get(c - IDM_RESTORE_BASE).cloned());
                        if let Some(snap) = snap {
                            restore_layout(hwnd, &snap);
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_TIMER => {
                match wparam.0 {
                    config::SAVE_TIMER_ID => {
                        let _ = KillTimer(hwnd, config::SAVE_TIMER_ID);
                        config::save_now();
                    }
                    TIMER_REFRESH => {
                        let _ = KillTimer(hwnd, TIMER_REFRESH);
                        on_desktop_changed(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            desktop::WM_DESKTOP_CHANGED => {
                // Coalesce event bursts; the timer does the real work.
                SetTimer(hwnd, TIMER_REFRESH, 300, None);
                LRESULT(0)
            }
            WM_OUTSIDE_CLICK => {
                fence::clear_all_selections();
                LRESULT(0)
            }
            WM_DESTROY => {
                // Flush any pending debounced save so the last mutation wins.
                config::save_now();
                remove_tray_icon(hwnd);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
