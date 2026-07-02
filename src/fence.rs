// Fence window: borderless WS_POPUP pinned to the desktop layer.
// M1: creation, HWND_BOTTOM pinning via WM_WINDOWPOSCHANGING, painting.
// M2: move (title-bar drag), resize (8px border band), roll-up (double-click
// title bar).
// M3: geometry syncs into config on WM_WINDOWPOSCHANGED.
// M4: icon grid hit-testing, double-click launches items.
// M5: icon drag between/within fences (ghost cursor), right-click title bar
// menu with Delete fence; id->hwnd registry so fences can repaint each other.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, EndPaint, EnumDisplayMonitors, InvalidateRect, MonitorFromWindow,
    ScreenToClient, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{self, FenceConfig};
use crate::icons;
use crate::render::{self, FenceRenderer};

const FENCE_CLASS: PCWSTR = w!("OrbirusFence");
const RESIZE_BAND: i32 = 8; // physical px, per spec §7
const MIN_FENCE_WIDTH: i32 = 120;
const IDM_FENCE_DELETE: usize = 100;

struct DragState {
    path: String,
    start: POINT, // client px
    active: bool,
    cursor: Option<HCURSOR>,
}

pub struct FenceState {
    pub id: String,
    pub title: String,
    pub color: D2D1_COLOR_F,
    pub opacity: f32,
    pub corner_radius: f32,
    pub rolled_up: bool,
    pub restore_height: i32,
    renderer: Option<FenceRenderer>,
    drag: Option<DragState>,
}

thread_local! {
    // fence id -> HWND, so any fence can invalidate another (drop targets,
    // items returning to Unsorted).
    static REGISTRY: RefCell<HashMap<String, isize>> = RefCell::new(HashMap::new());
}

pub fn hwnd_for(id: &str) -> Option<HWND> {
    REGISTRY.with(|r| r.borrow().get(id).map(|&h| HWND(h as *mut _)))
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
        drag: None,
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

    REGISTRY.with(|r| r.borrow_mut().insert(cfg.id.clone(), hwnd.0 as isize));

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

unsafe fn state_mut(hwnd: HWND) -> Option<&'static mut FenceState> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceState;
    ptr.as_mut()
}

unsafe fn titlebar_height_px(hwnd: HWND) -> i32 {
    let dpi = GetDpiForWindow(hwnd) as f32;
    (render::TITLEBAR_HEIGHT * dpi / 96.0).round() as i32
}

/// The fence's items and the global icon size, straight from config.
unsafe fn fence_items(id: &str) -> (Vec<String>, f32) {
    config::with(|c| {
        let items = c
            .fences
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.items.clone())
            .unwrap_or_default();
        (items, c.icon_size as f32)
    })
}

/// Grid cell index under a client-space point, or None outside the grid.
unsafe fn icon_index_at(hwnd: HWND, id: &str, client_x: i32, client_y: i32) -> Option<usize> {
    let to_dip = 96.0 / GetDpiForWindow(hwnd) as f32;
    let mut rc = RECT::default();
    GetClientRect(hwnd, &mut rc).ok()?;
    let (items, icon_size) = fence_items(id);
    let metrics = render::grid_metrics((rc.right - rc.left) as f32 * to_dip, icon_size);
    let i = metrics.index_at(client_x as f32 * to_dip, client_y as f32 * to_dip)?;
    (i < items.len()).then_some(i)
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

/// The fence window (any fence, including the source) under a screen point.
unsafe fn fence_at_point(pt: POINT) -> Option<(String, HWND)> {
    REGISTRY.with(|r| {
        for (id, &h) in r.borrow().iter() {
            let hwnd = HWND(h as *mut _);
            let mut rc = RECT::default();
            if GetWindowRect(hwnd, &mut rc).is_ok()
                && pt.x >= rc.left
                && pt.x < rc.right
                && pt.y >= rc.top
                && pt.y < rc.bottom
            {
                return Some((id.clone(), hwnd));
            }
        }
        None
    })
}

unsafe fn end_drag_cursor(cursor: Option<HCURSOR>) {
    if let Some(cur) = cursor {
        if let Ok(arrow) = LoadCursorW(None, IDC_ARROW) {
            SetCursor(arrow);
        }
        let _ = DestroyCursor(cur);
    }
}

/// Drop the dragged item at a screen point: reorder within the source fence,
/// or reassign into another fence (at the drop cell when it's inside the
/// grid, else appended). Anywhere else is a no-op — the icon snaps back.
unsafe fn complete_drop(src_hwnd: HWND, src_id: &str, drag: &DragState, screen_pt: POINT) {
    let Some((target_id, target_hwnd)) = fence_at_point(screen_pt) else {
        return;
    };
    let mut cpt = screen_pt;
    let _ = ScreenToClient(target_hwnd, &mut cpt);
    let to_dip = 96.0 / GetDpiForWindow(target_hwnd) as f32;
    let mut rc = RECT::default();
    let _ = GetClientRect(target_hwnd, &mut rc);
    let icon_size = config::with(|c| c.icon_size as f32);
    let metrics = render::grid_metrics((rc.right - rc.left) as f32 * to_dip, icon_size);
    let drop_idx = metrics.index_at(cpt.x as f32 * to_dip, cpt.y as f32 * to_dip);

    let changed = config::with(|cfg| {
        if src_id == target_id {
            let Some(f) = cfg.fences.iter_mut().find(|f| f.id == src_id) else {
                return false;
            };
            let Some(from) = f.items.iter().position(|p| p == &drag.path) else {
                return false;
            };
            let Some(di) = drop_idx else { return false };
            let to = di.min(f.items.len().saturating_sub(1));
            if to == from {
                return false;
            }
            let item = f.items.remove(from);
            f.items.insert(to.min(f.items.len()), item);
            true
        } else {
            let Some(sf) = cfg.fences.iter_mut().find(|f| f.id == src_id) else {
                return false;
            };
            let Some(from) = sf.items.iter().position(|p| p == &drag.path) else {
                return false;
            };
            let item = sf.items.remove(from);
            let Some(tf) = cfg.fences.iter_mut().find(|f| f.id == target_id) else {
                return false;
            };
            match drop_idx {
                Some(di) if di < tf.items.len() => tf.items.insert(di, item),
                _ => tf.items.push(item),
            }
            true
        }
    });
    if changed {
        config::schedule_save();
        let _ = InvalidateRect(src_hwnd, None, false);
        let _ = InvalidateRect(target_hwnd, None, false);
    }
}

/// Right-click title-bar menu. Delete is disabled for an "Unsorted" fence
/// that still holds items (§5) — everything else deletes, items returning
/// to Unsorted.
unsafe fn show_fence_menu(hwnd: HWND) {
    let Some(state) = state_mut(hwnd) else { return };
    let deletable = config::with(|cfg| {
        cfg.fences
            .iter()
            .find(|f| f.id == state.id)
            .map(|f| f.title != "Unsorted" || f.items.is_empty())
            .unwrap_or(false)
    });
    let Ok(menu) = CreatePopupMenu() else { return };
    let flags = if deletable {
        MF_STRING
    } else {
        MF_STRING | MF_GRAYED
    };
    let _ = AppendMenuW(menu, flags, IDM_FENCE_DELETE, w!("Delete fence"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Required for the menu to dismiss properly on a WS_EX_NOACTIVATE window.
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
    if cmd.0 as usize == IDM_FENCE_DELETE {
        delete_fence(hwnd);
    }
}

unsafe fn delete_fence(hwnd: HWND) {
    let Some(state) = state_mut(hwnd) else { return };
    let id = state.id.clone();
    // Some(receiver): deleted; receiver is the Unsorted id if items moved.
    let outcome = config::with(|cfg| {
        let idx = cfg.fences.iter().position(|f| f.id == id)?;
        if cfg.fences[idx].title == "Unsorted" && !cfg.fences[idx].items.is_empty() {
            return None;
        }
        let removed = cfg.fences.remove(idx);
        if removed.items.is_empty() {
            return Some(None);
        }
        let u = config::ensure_unsorted(cfg);
        cfg.fences[u].items.extend(removed.items);
        Some(Some(cfg.fences[u].id.clone()))
    });
    let Some(receiver) = outcome else { return };
    config::schedule_save();
    if let Some(uid) = receiver {
        match hwnd_for(&uid) {
            Some(h) => {
                let _ = InvalidateRect(h, None, false);
            }
            None => {
                // Unsorted was just created by ensure_unsorted: give it a window.
                let fc = config::with(|c| c.fences.iter().find(|f| f.id == uid).cloned());
                if let (Some(fc), Ok(hmodule)) = (fc, GetModuleHandleW(None)) {
                    let _ = create_fence(hmodule.into(), &fc);
                }
            }
        }
    }
    let _ = DestroyWindow(hwnd);
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
            WM_LBUTTONDOWN => {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if y < titlebar_height_px(hwnd) {
                    // Title-bar drag moves the fence via the system move loop.
                    let _ = ReleaseCapture();
                    SendMessageW(
                        hwnd,
                        WM_NCLBUTTONDOWN,
                        WPARAM(HTCAPTION as usize),
                        LPARAM(0),
                    );
                } else if let Some(state) = state_mut(hwnd) {
                    // Body click on an icon: candidate item drag. It only
                    // becomes one after passing the system drag threshold,
                    // so double-click launch still works.
                    if let Some(i) = icon_index_at(hwnd, &state.id, x, y) {
                        let (items, _) = fence_items(&state.id);
                        if let Some(item) = items.get(i) {
                            state.drag = Some(DragState {
                                path: item.clone(),
                                start: POINT { x, y },
                                active: false,
                                cursor: None,
                            });
                            SetCapture(hwnd);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                if let Some(state) = state_mut(hwnd) {
                    if let Some(drag) = &mut state.drag {
                        let x = (lparam.0 & 0xFFFF) as i16 as i32;
                        let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                        if !drag.active
                            && ((x - drag.start.x).abs() > GetSystemMetrics(SM_CXDRAG)
                                || (y - drag.start.y).abs() > GetSystemMetrics(SM_CYDRAG))
                        {
                            drag.active = true;
                            drag.cursor = icons::drag_cursor(&drag.path);
                        }
                        if drag.active {
                            if let Some(cur) = drag.cursor {
                                SetCursor(cur);
                            }
                        }
                    }
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let taken = state_mut(hwnd).and_then(|s| s.drag.take());
                if let Some(drag) = taken {
                    let _ = ReleaseCapture();
                    if drag.active {
                        end_drag_cursor(drag.cursor);
                        let mut pt = POINT {
                            x: (lparam.0 & 0xFFFF) as i16 as i32,
                            y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                        };
                        let _ = ClientToScreen(hwnd, &mut pt);
                        if let Some(state) = state_mut(hwnd) {
                            let src_id = state.id.clone();
                            complete_drop(hwnd, &src_id, &drag, pt);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                if let Some(state) = state_mut(hwnd) {
                    if let Some(drag) = state.drag.take() {
                        end_drag_cursor(drag.cursor);
                    }
                }
                LRESULT(0)
            }
            WM_LBUTTONDBLCLK => {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if y < titlebar_height_px(hwnd) {
                    toggle_rollup(hwnd);
                } else if let Some(state) = state_mut(hwnd) {
                    if let Some(i) = icon_index_at(hwnd, &state.id, x, y) {
                        let (items, _) = fence_items(&state.id);
                        if let Some(item) = items.get(i) {
                            crate::launch::launch(item);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if y < titlebar_height_px(hwnd) {
                    show_fence_menu(hwnd);
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
                        let (items, icon_size) = fence_items(&state.id);
                        let ok = renderer
                            .draw(
                                &state.title,
                                state.color,
                                state.opacity,
                                state.corner_radius,
                                &items,
                                icon_size,
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
                    let state = Box::from_raw(ptr);
                    REGISTRY.with(|r| r.borrow_mut().remove(&state.id));
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
