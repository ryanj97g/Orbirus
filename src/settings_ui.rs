// v1.2: a real Settings window from the tray (replaces open-config-in-
// Notepad). Four labeled combos (Color / Opacity / Corner radius / Icon
// size — all universal) that apply immediately, plus a Start with Windows
// checkbox and Close.

use std::cell::Cell;
use std::ffi::c_void;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, GetMonitorInfoW, MonitorFromWindow, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, COLOR_BTNFACE, DEFAULT_CHARSET, HBRUSH, HFONT, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, OUT_DEFAULT_PRECIS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config;
use crate::fence;

const SETTINGS_CLASS: PCWSTR = w!("OrbirusSettings");

const ID_CLOSE: usize = 2; // Esc lands here via IsDialogMessageW
const ID_COLOR: usize = 20;
const ID_OPACITY: usize = 21;
const ID_RADIUS: usize = 22;
const ID_ICONSIZE: usize = 23;
const ID_AUTOSTART: usize = 24;

const CB_ADDSTRING: u32 = 0x0143;
const CB_GETCURSEL: u32 = 0x0147;
const CB_SETCURSEL: u32 = 0x014E;
const CBN_SELCHANGE: u32 = 1;
const BM_SETCHECK: u32 = 0x00F1;
const BM_GETCHECK: u32 = 0x00F0;

thread_local! {
    static DIALOG_HWND: Cell<isize> = const { Cell::new(0) };
}

pub fn dialog_hwnd() -> HWND {
    DIALOG_HWND.with(|c| HWND(c.get() as *mut _))
}

pub unsafe fn register_class(hinstance: HINSTANCE) -> Result<()> {
    let wc = WNDCLASSW {
        lpfnWndProc: Some(settings_wndproc),
        hInstance: hinstance,
        lpszClassName: SETTINGS_CLASS,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as isize as *mut c_void),
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return Err(Error::from_win32());
    }
    Ok(())
}

struct Ctx {
    font: isize,
}

pub unsafe fn open(owner: HWND) {
    let existing = DIALOG_HWND.with(|c| c.get());
    if existing != 0 {
        let _ = SetForegroundWindow(HWND(existing as *mut _));
        return;
    }
    let Ok(hmodule) = GetModuleHandleW(None) else { return };
    let hinstance: HINSTANCE = hmodule.into();
    let dpi = GetDpiForWindow(owner).max(96) as i32;
    let s = |v: i32| v * dpi / 96;

    let (dw, dh) = (s(320), s(260));
    let hmon = MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let _ = GetMonitorInfoW(hmon, &mut mi);
    let dx = (mi.rcWork.left + mi.rcWork.right) / 2 - dw / 2;
    let dy = (mi.rcWork.top + mi.rcWork.bottom) / 2 - dh / 2;
    let dlg = match CreateWindowExW(
        WS_EX_TOOLWINDOW,
        SETTINGS_CLASS,
        w!("Orbirus Settings"),
        WS_POPUP | WS_CAPTION | WS_SYSMENU,
        dx,
        dy,
        dw,
        dh,
        None,
        None,
        hinstance,
        None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };

    let font = CreateFontW(
        -s(15),
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        0,
        w!("Segoe UI"),
    );

    let mk = |class: PCWSTR,
              text: PCWSTR,
              style: WINDOW_STYLE,
              x: i32,
              y: i32,
              w: i32,
              h: i32,
              id: usize|
     -> HWND {
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            text,
            WS_CHILD | WS_VISIBLE | style,
            s(x),
            s(y),
            s(w),
            s(h),
            dlg,
            HMENU(id as *mut c_void),
            hinstance,
            None,
        )
        .unwrap_or_default();
        SendMessageW(hwnd, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        hwnd
    };
    let combo_style = WINDOW_STYLE((CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32) | WS_VSCROLL;

    let rows: [(PCWSTR, usize); 4] = [
        (w!("Color"), ID_COLOR),
        (w!("Opacity"), ID_OPACITY),
        (w!("Corner radius"), ID_RADIUS),
        (w!("Icon size"), ID_ICONSIZE),
    ];
    for (i, (label, id)) in rows.iter().enumerate() {
        let y = 14 + i as i32 * 36;
        let _ = mk(w!("STATIC"), *label, WINDOW_STYLE(0), 12, y + 4, 110, 20, 0);
        let combo = mk(w!("COMBOBOX"), PCWSTR::null(), combo_style, 130, y, 168, 200, *id);
        let (labels, cur): (Vec<PCWSTR>, usize) = match *id {
            ID_COLOR => {
                let cur_hex = config::with(|c| c.fences.first().map(|f| f.color.clone()))
                    .and_then(|s| u32::from_str_radix(s.trim_start_matches('#'), 16).ok())
                    .unwrap_or(0x1E1E2E);
                let pos = fence::PALETTE
                    .iter()
                    .position(|(_, h)| *h == cur_hex)
                    .unwrap_or(0);
                (fence::PALETTE.iter().map(|(n, _)| *n).collect(), pos)
            }
            ID_OPACITY => {
                let cur = config::with(|c| c.fences.first().map(|f| f.opacity).unwrap_or(0.78));
                let pct = (cur * 100.0).round() as u32;
                let pos = fence::OPACITIES
                    .iter()
                    .position(|(_, p)| *p == pct)
                    .unwrap_or(2);
                (fence::OPACITIES.iter().map(|(n, _)| *n).collect(), pos)
            }
            ID_RADIUS => {
                let cur = config::with(|c| {
                    c.fences.first().map(|f| f.corner_radius).unwrap_or(12.0)
                })
                .round() as u32;
                let pos = fence::RADII.iter().position(|(_, r)| *r == cur).unwrap_or(2);
                (fence::RADII.iter().map(|(n, _)| *n).collect(), pos)
            }
            _ => {
                let cur = config::with(|c| c.icon_size);
                let pos = fence::ICON_SIZES
                    .iter()
                    .position(|(_, v)| *v == cur)
                    .unwrap_or(1);
                (fence::ICON_SIZES.iter().map(|(n, _)| *n).collect(), pos)
            }
        };
        for l in labels {
            SendMessageW(combo, CB_ADDSTRING, WPARAM(0), LPARAM(l.as_ptr() as isize));
        }
        SendMessageW(combo, CB_SETCURSEL, WPARAM(cur), LPARAM(0));
    }

    let check = mk(
        w!("BUTTON"),
        w!("Start with Windows"),
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
        12,
        162,
        200,
        24,
        ID_AUTOSTART,
    );
    SendMessageW(
        check,
        BM_SETCHECK,
        WPARAM(if crate::autostart_enabled() { 1 } else { 0 }),
        LPARAM(0),
    );
    let _ = mk(
        w!("BUTTON"),
        w!("Close"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
        216,
        196,
        82,
        26,
        ID_CLOSE,
    );

    let ctx = Box::new(Ctx { font: font.0 as isize });
    SetWindowLongPtrW(dlg, GWLP_USERDATA, Box::into_raw(ctx) as isize);
    DIALOG_HWND.with(|c| c.set(dlg.0 as isize));
    let _ = ShowWindow(dlg, SW_SHOW);
    let _ = SetForegroundWindow(dlg);
}

extern "system" fn settings_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = wparam.0 & 0xFFFF;
                let code = (wparam.0 >> 16) as u32;
                let combo = HWND(lparam.0 as *mut _);
                match id {
                    ID_CLOSE | 1 => {
                        let _ = DestroyWindow(hwnd);
                    }
                    ID_AUTOSTART => {
                        let checked =
                            SendMessageW(combo, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == 1;
                        crate::set_autostart(checked);
                    }
                    ID_COLOR if code == CBN_SELCHANGE => {
                        let i = SendMessageW(combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
                        if let Some((_, hex)) = fence::PALETTE.get(i as usize) {
                            fence::set_all_color(*hex);
                        }
                    }
                    ID_OPACITY if code == CBN_SELCHANGE => {
                        let i = SendMessageW(combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
                        if let Some((_, pct)) = fence::OPACITIES.get(i as usize) {
                            fence::set_all_opacity(*pct);
                        }
                    }
                    ID_RADIUS if code == CBN_SELCHANGE => {
                        let i = SendMessageW(combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
                        if let Some((_, r)) = fence::RADII.get(i as usize) {
                            fence::set_all_radius(*r);
                        }
                    }
                    ID_ICONSIZE if code == CBN_SELCHANGE => {
                        let i = SendMessageW(combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
                        if let Some((_, v)) = fence::ICON_SIZES.get(i as usize) {
                            fence::set_icon_size(hwnd, *v);
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                DIALOG_HWND.with(|c| c.set(0));
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ctx;
                if !ptr.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let ctx = Box::from_raw(ptr);
                    let _ = DeleteObject(HFONT(ctx.font as *mut _));
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
