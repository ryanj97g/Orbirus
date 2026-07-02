// Fence window: borderless WS_POPUP pinned to the desktop layer.
// M1: creation, HWND_BOTTOM pinning via WM_WINDOWPOSCHANGING, painting.
// M2: move (title-bar drag), resize (8px border band), roll-up (double-click
// title bar).

use std::ffi::c_void;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, EnumDisplayMonitors, InvalidateRect, MonitorFromWindow, HDC, HMONITOR,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{self, FenceConfig};
use crate::render::{self, FenceRenderer};

const FENCE_CLASS: PCWSTR = w!("OrbirusFence");
const RESIZE_BAND: i32 = 8; // physical px, per spec §7
const MIN_FENCE_WIDTH: i32 = 120;

pub struct FenceState {
    pub id: String,
    pub title: String,
    pub color: D2D1_COLOR_F,
    pub opacity: f32,
    pub corner_radius: f32,
    pub rolled_up: bool,
    pub restore_height: i32,
    renderer: Option<FenceRenderer>,
}

pub fn color_from_hex(hex: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a: 1.0,
    }
}

/// Parses "#RRGGBB"; falls back to the default fence color on bad input.
pub fn parse_color(s: &str) -> D2D1_COLOR_F {
    let hex = s
        .strip_prefix('#')
        .and_then(|v| u32::from_str_radix(v, 16).ok())
        .filter(|_| s.len() == 7)
        .unwrap_or(0x1E1E2E);
    color_from_hex(hex)
}

pub unsafe fn register_class(hinstance: HINSTANCE) -> Result<()> {
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
        lpfnWndProc: Some(fence_wndproc),
        hInstance: hinstance,
        lpszClassName: FENCE_CLASS,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return Err(Error::from_win32());
    }
    Ok(())
}

pub unsafe fn create_fence(hinstance: HINSTANCE, cfg: &FenceConfig) -> Result<HWND> {
    let state = Box::new(FenceState {
        id: cfg.id.clone(),
        title: cfg.title.clone(),
        color: parse_color(&cfg.color),
        opacity: cfg.opacity,
        corner_radius: cfg.corner_radius,
        rolled_up: cfg.rolled_up,
        restore_height: cfg.h,
        renderer: None,
    });

    let mut title_utf16: Vec<u16> = cfg.title.encode_utf16().collect();
    title_utf16.push(0);

    // WS_THICKFRAME makes DefWindowProc run the system resize loop for our
    // WM_NCHITTEST border results; WM_NCCALCSIZE below removes its visible
    // frame entirely.
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
        FENCE_CLASS,
        PCWSTR(title_utf16.as_ptr()),
        WS_POPUP | WS_THICKFRAME,
        cfg.x,
        cfg.y,
        cfg.w,
        cfg.h,
        None,
        None,
        hinstance,
        Some(Box::into_raw(state) as *const c_void),
    )?;

    SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)?;
    if cfg.rolled_up {
        SetWindowPos(
            hwnd,
            HWND_BOTTOM,
            0,
            0,
            cfg.w,
            titlebar_height_px(hwnd),
            SWP_NOMOVE | SWP_NOACTIVATE,
        )?;
    }
    SetWindowPos(
        hwnd,
        HWND_BOTTOM,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
    )?;

    Ok(hwnd)
}

unsafe fn monitor_index(hwnd: HWND) -> u32 {
    unsafe extern "system" fn collect(
        mon: HMONITOR,
        _dc: HDC,
        _rc: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let list = &mut *(lparam.0 as *mut Vec<isize>);
        list.push(mon.0 as isize);
        TRUE
    }
    let target = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut monitors: Vec<isize> = Vec::new();
    let _ = EnumDisplayMonitors(
        None,
        None,
        Some(collect),
        LPARAM(&mut monitors as *mut _ as isize),
    );
    monitors
        .iter()
        .position(|&m| m == target.0 as isize)
        .unwrap_or(0) as u32
}

/// Mirrors the window's current geometry into the live config; schedules a
/// debounced save only when something actually changed.
unsafe fn sync_to_config(hwnd: HWND) {
    let Some(state) = state_mut(hwnd) else { return };
    let mut rc = RECT::default();
    if GetWindowRect(hwnd, &mut rc).is_err() {
        return;
    }
    // A rolled-up fence keeps its restored height in config.
    let h = if state.rolled_up {
        state.restore_height
    } else {
        rc.bottom - rc.top
    };
    let monitor = monitor_index(hwnd);
    let changed = config::with(|cfg| {
        let Some(f) = cfg.fences.iter_mut().find(|f| f.id == state.id) else {
            return false;
        };
        let new = (rc.left, rc.top, rc.right - rc.left, h, state.rolled_up, monitor);
        if new == (f.x, f.y, f.w, f.h, f.rolled_up, f.monitor) {
            return false;
        }
        (f.x, f.y, f.w, f.h, f.rolled_up, f.monitor) = new;
        true
    });
    if changed {
        config::schedule_save();
    }
}

unsafe fn state_mut(hwnd: HWND) -> Option<&'static mut FenceState> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceState;
    ptr.as_mut()
}

unsafe fn titlebar_height_px(hwnd: HWND) -> i32 {
    let dpi = GetDpiForWindow(hwnd) as f32;
    (render::TITLEBAR_HEIGHT * dpi / 96.0).round() as i32
}

unsafe fn toggle_rollup(hwnd: HWND) {
    let mut rc = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rc);
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    let Some(state) = state_mut(hwnd) else { return };

    if state.rolled_up {
        state.rolled_up = false;
        let _ = SetWindowPos(
            hwnd,
            HWND_BOTTOM,
            0,
            0,
            w,
            state.restore_height,
            SWP_NOMOVE | SWP_NOACTIVATE,
        );
    } else {
        state.rolled_up = true;
        state.restore_height = h;
        let _ = SetWindowPos(
            hwnd,
            HWND_BOTTOM,
            0,
            0,
            w,
            titlebar_height_px(hwnd),
            SWP_NOMOVE | SWP_NOACTIVATE,
        );
    }
}

extern "system" fn fence_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCREATE => {
                let cs = lparam.0 as *const CREATESTRUCTW;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // Permanently pin to the bottom of the z-order: whatever tries to
            // raise this window, it stays behind all normal windows.
            WM_WINDOWPOSCHANGING => {
                let pos = lparam.0 as *mut WINDOWPOS;
                (*pos).hwndInsertAfter = HWND_BOTTOM;
                (*pos).flags = ((*pos).flags | SWP_NOACTIVATE) & !SWP_NOZORDER;
                LRESULT(0)
            }
            // Client area covers the whole window: WS_THICKFRAME contributes
            // resize behavior but no visible frame.
            WM_NCCALCSIZE if wparam.0 != 0 => LRESULT(0),
            WM_NCHITTEST => {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let mut rc = RECT::default();
                let _ = GetWindowRect(hwnd, &mut rc);
                let left = x < rc.left + RESIZE_BAND;
                let right = x >= rc.right - RESIZE_BAND;
                let top = y < rc.top + RESIZE_BAND;
                let bottom = y >= rc.bottom - RESIZE_BAND;
                let ht = match (left, right, top, bottom) {
                    (true, _, true, _) => HTTOPLEFT,
                    (_, true, true, _) => HTTOPRIGHT,
                    (true, _, _, true) => HTBOTTOMLEFT,
                    (_, true, _, true) => HTBOTTOMRIGHT,
                    (true, _, _, _) => HTLEFT,
                    (_, true, _, _) => HTRIGHT,
                    (_, _, true, _) => HTTOP,
                    (_, _, _, true) => HTBOTTOM,
                    _ => HTCLIENT,
                };
                LRESULT(ht as isize)
            }
            WM_GETMINMAXINFO => {
                let mmi = lparam.0 as *mut MINMAXINFO;
                (*mmi).ptMinTrackSize.x = MIN_FENCE_WIDTH;
                (*mmi).ptMinTrackSize.y = titlebar_height_px(hwnd);
                LRESULT(0)
            }
            // Title-bar drag moves the fence: hand the click to the system
            // move loop as a caption drag (we're WS_POPUP, no real caption).
            WM_LBUTTONDOWN => {
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if y < titlebar_height_px(hwnd) {
                    let _ = ReleaseCapture();
                    SendMessageW(
                        hwnd,
                        WM_NCLBUTTONDOWN,
                        WPARAM(HTCAPTION as usize),
                        LPARAM(0),
                    );
                }
                LRESULT(0)
            }
            WM_LBUTTONDBLCLK => {
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if y < titlebar_height_px(hwnd) {
                    toggle_rollup(hwnd);
                }
                LRESULT(0)
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            // Layout mutations (move/resize/roll-up) flow into the config
            // here. DefWindowProc still runs so WM_SIZE/WM_MOVE are generated.
            WM_WINDOWPOSCHANGED => {
                sync_to_config(hwnd);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                if let Some(state) = state_mut(hwnd) {
                    if state.renderer.is_none() {
                        state.renderer = FenceRenderer::new(hwnd).ok();
                    }
                    if let Some(renderer) = &state.renderer {
                        let ok = renderer
                            .draw(
                                &state.title,
                                state.color,
                                state.opacity,
                                state.corner_radius,
                            )
                            .is_ok();
                        if !ok {
                            // Device loss (D2DERR_RECREATE_TARGET): rebuild on
                            // the next paint.
                            state.renderer = None;
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_SIZE => {
                if let Some(state) = state_mut(hwnd) {
                    if let Some(renderer) = &state.renderer {
                        renderer.resize(
                            (lparam.0 & 0xFFFF) as u32,
                            ((lparam.0 >> 16) & 0xFFFF) as u32,
                        );
                    }
                }
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let new_dpi = (wparam.0 & 0xFFFF) as f32;
                if let Some(state) = state_mut(hwnd) {
                    if let Some(renderer) = &state.renderer {
                        renderer.set_dpi(new_dpi);
                    }
                }
                let suggested = lparam.0 as *const RECT;
                let _ = SetWindowPos(
                    hwnd,
                    HWND_BOTTOM,
                    (*suggested).left,
                    (*suggested).top,
                    (*suggested).right - (*suggested).left,
                    (*suggested).bottom - (*suggested).top,
                    SWP_NOACTIVATE,
                );
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceState;
                if !ptr.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(ptr));
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
