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

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// WM_APP + 1 is reserved for desktop-change notifications (M7 file watcher).
const WM_TRAYICON: u32 = WM_APP + 2;
const IDM_EXIT: usize = 1;
const IDM_NEW_FENCE: usize = 2;
const IDM_SETTINGS: usize = 3;
const IDM_SORT_UNSORTED: usize = 4;
const TRAY_UID: u32 = 1;
// Debounces watcher bursts (Explorer fires several events per operation).
const TIMER_REFRESH: usize = 2;

fn main() -> Result<()> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?;
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;

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
        let fence_cfgs = config::with(|c| c.fences.clone());
        for fc in &fence_cfgs {
            fence::create_fence(hinstance, fc)?;
        }
        println!("Loaded {} fence(s) from config.", fence_cfgs.len());

        // M7: live updates from the real Desktop folders.
        desktop::start_watcher(hwnd);

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
            let handled = [fence::rename_dialog_hwnd(), rules_ui::dialog_hwnd()]
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
/// Unsorted; confirm before moving. Manual placements elsewhere are never
/// touched.
unsafe fn sort_unsorted_now(hwnd: HWND) {
    let moves: Vec<(String, String)> = config::with(|cfg| {
        let Some(u) = cfg.fences.iter().position(|f| f.title == "Unsorted") else {
            return Vec::new();
        };
        let uid = cfg.fences[u].id.clone();
        let items = cfg.fences[u].items.clone();
        let cfg_ref = &*cfg;
        items
            .into_iter()
            .filter_map(|item| {
                rules::match_fence(std::path::Path::new(&item), cfg_ref)
                    .filter(|target| *target != uid)
                    .map(|target| (item, target))
            })
            .collect()
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
    let text: Vec<u16> = format!("This will move {} items. Continue?", moves.len())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let answer = MessageBoxW(hwnd, PCWSTR(text.as_ptr()), w!("Orbirus"), MB_YESNO | MB_ICONQUESTION);
    if answer != IDYES {
        return;
    }
    config::with(|cfg| {
        for (item, target_id) in &moves {
            let Some(u) = cfg.fences.iter().position(|f| f.title == "Unsorted") else {
                return;
            };
            if let Some(pos) = cfg.fences[u].items.iter().position(|p| p == item) {
                let it = cfg.fences[u].items.remove(pos);
                if let Some(t) = cfg.fences.iter_mut().find(|f| f.id == *target_id) {
                    t.items.push(it);
                }
            }
        }
    });
    config::schedule_save();
    fence::invalidate_all();
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
    let _ = fence::create_fence(hmodule.into(), &fc);
}

unsafe fn add_tray_icon(hwnd: HWND) -> Result<()> {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAYICON,
        hIcon: LoadIconW(None, IDI_APPLICATION)?,
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
                    IDM_SETTINGS => launch::open_config_in_notepad(),
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
