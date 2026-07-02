// Orbirus — M0 skeleton: DPI awareness, COM init, tray icon with Exit, message loop.
// NOTE: #![windows_subsystem = "windows"] is intentionally absent until M7 —
// keep the console during development for println! debugging.

mod config;
mod desktop;
mod fence;
mod icons;
mod launch;
mod render;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
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
const TRAY_UID: u32 = 1;

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
            _ => config::Config::default(),
        };
        config::init(cfg, hwnd);

        // M4: everything on the real Desktop folders appears exactly once;
        // unassigned items land in "Unsorted".
        let desktop_items = desktop::enumerate();
        let changed = desktop::reconcile(&desktop_items);
        if first_run || changed {
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
        let fence_cfgs = config::with(|c| c.fences.clone());
        for fc in &fence_cfgs {
            fence::create_fence(hinstance, fc)?;
        }
        println!("Loaded {} fence(s) from config.", fence_cfgs.len());

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
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        CoUninitialize();
        Ok(())
    }
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
                    IDM_SETTINGS => launch::open_config_in_notepad(),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == config::SAVE_TIMER_ID {
                    let _ = KillTimer(hwnd, config::SAVE_TIMER_ID);
                    config::save_now();
                }
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
