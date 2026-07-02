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
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, EndPaint, EnumDisplayMonitors, MonitorFromWindow, ScreenToClient,
    HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Controls::{
    TOOLTIPS_CLASSW, TTF_ABSOLUTE, TTF_TRACK, TTM_ADDTOOLW, TTM_SETMAXTIPWIDTH,
    TTM_TRACKACTIVATE, TTM_TRACKPOSITION, TTM_UPDATETIPTEXTW, TTS_ALWAYSTIP, TTS_NOPREFIX,
    TTTOOLINFOW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};

// Edit-control message (we avoid pulling in more of Win32_UI_Controls' edit API).
const EM_SETSEL: u32 = 0x00B1;
// Posted by TrackMouseEvent(TME_LEAVE); not exported by our feature set.
const WM_MOUSELEAVE: u32 = 0x02A3;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{self, FenceConfig};
use crate::icons;
use crate::render::{self, FenceRenderer};

const FENCE_CLASS: PCWSTR = w!("OrbirusFence");
const RENAME_CLASS: PCWSTR = w!("OrbirusRename");
const RESIZE_BAND: i32 = 8; // physical px, per spec §7
const MIN_FENCE_WIDTH: i32 = 120;

const IDM_FENCE_DELETE: usize = 100;
const IDM_FENCE_RENAME: usize = 101;
const IDM_FENCE_AUTOORG: usize = 102;
const IDM_FENCE_RAINBOW: usize = 103;
const TIMER_SCROLLBAR: usize = 7;
const IDM_COLOR_BASE: usize = 110; // ..117
const IDM_OPACITY_BASE: usize = 120; // ..123
const IDM_RADIUS_BASE: usize = 130; // ..133

// Fixed palette (§7: 8 colors).
pub(crate) const PALETTE: [(PCWSTR, u32); 8] = [
    (w!("Midnight"), 0x1E1E2E),
    (w!("Charcoal"), 0x111111),
    (w!("Slate"), 0x334155),
    (w!("Ocean"), 0x0C4A6E),
    (w!("Forest"), 0x14532D),
    (w!("Plum"), 0x581C87),
    (w!("Wine"), 0x7F1D1D),
    (w!("Amber"), 0x78350F),
];
// Opacity presets (§7: 50/65/78/90%).
pub(crate) const OPACITIES: [(PCWSTR, u32); 4] = [
    (w!("50%"), 50),
    (w!("65%"), 65),
    (w!("78%"), 78),
    (w!("90%"), 90),
];
// Corner radius presets (§7: 0/6/12/20).
pub(crate) const RADII: [(PCWSTR, u32); 4] =
    [(w!("0"), 0), (w!("6"), 6), (w!("12"), 12), (w!("20"), 20)];
// M11: global icon size presets.
pub(crate) const ICON_SIZES: [(PCWSTR, u32); 3] =
    [(w!("Small"), 32), (w!("Medium"), 48), (w!("Large"), 64)];
const IDM_ICONSIZE_BASE: usize = 140; // ..142

struct DragState {
    // M13: dragging moves every selected item together.
    paths: Vec<String>,
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
    // M10: grid scroll offset (DIPs, runtime-only), hovered cell, and the
    // tracking tooltip that shows full names for truncated labels.
    scroll_y: f32,
    hover: Option<usize>,
    tracking_mouse: bool,
    tooltip: isize,
    tooltip_text: Vec<u16>,
    // M11: rolled fence temporarily expanded under the cursor. peek_armed
    // gates it: arming happens on mouse-leave, so rolling a fence up under
    // your own cursor doesn't instantly peek it back open.
    peeking: bool,
    peek_armed: bool,
    // M13: multi-select — selected item paths and an in-progress
    // rubber-band (start, current) in client px.
    selected: Vec<String>,
    band: Option<(POINT, POINT)>,
    // v1.2: scrollbar shows only while scrolling.
    scrollbar_visible: bool,
    // v1.2: temporarily pushed down by an unrolling fence — (original y,
    // id of the fence that displaced us). sync_to_config skips us while set.
    displaced: Option<(i32, String)>,
}

thread_local! {
    // fence id -> HWND, so any fence can invalidate another (drop targets,
    // items returning to Unsorted).
    static REGISTRY: RefCell<HashMap<String, isize>> = RefCell::new(HashMap::new());
    // The one open rename dialog (0 = none); the main loop feeds it through
    // IsDialogMessageW for Enter/Esc/Tab handling.
    static RENAME_HWND: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    // M11: current drag drop target — (hwnd, caret index or -1 for a
    // cross-fence ring); (0, 0) = none.
    static DROP_TARGET: std::cell::Cell<(isize, i32)> = const { std::cell::Cell::new((0, 0)) };
}

/// M11: updates the drop-target indicator, repainting old and new targets.
unsafe fn set_drop_target(new: Option<(HWND, i32)>) {
    let newv = new.map(|(h, i)| (h.0 as isize, i)).unwrap_or((0, 0));
    let old = DROP_TARGET.with(|c| c.get());
    if old != newv {
        DROP_TARGET.with(|c| c.set(newv));
        if old.0 != 0 {
            paint_fence(HWND(old.0 as *mut _));
        }
        if newv.0 != 0 {
            paint_fence(HWND(newv.0 as *mut _));
        }
    }
}

/// M12: tears down every fence window (layout restore rebuilds from config).
pub unsafe fn destroy_all() {
    let handles: Vec<isize> = REGISTRY.with(|r| r.borrow().values().cloned().collect());
    for h in handles {
        let _ = DestroyWindow(HWND(h as *mut _));
    }
}

/// M11: renderers hold per-target icon bitmaps; dropping them forces a
/// rebuild from the (re-extracted) pixel cache — used when icon size changes.
pub fn reset_renderers() {
    REGISTRY.with(|r| {
        for &h in r.borrow().values() {
            unsafe {
                if let Some(state) = state_mut(HWND(h as *mut _)) {
                    state.renderer = None;
                }
            }
        }
    });
}

/// For the main message loop: the open rename dialog, if any.
pub fn rename_dialog_hwnd() -> HWND {
    RENAME_HWND.with(|c| HWND(c.get() as *mut _))
}

pub fn hwnd_for(id: &str) -> Option<HWND> {
    REGISTRY.with(|r| r.borrow().get(id).map(|&h| HWND(h as *mut _)))
}

/// Repaints every fence window (desktop contents changed). M13: layered
/// (ULW) windows sit outside the WM_PAINT invalidation model, so this
/// renders directly.
pub fn invalidate_all() {
    let handles: Vec<isize> = REGISTRY.with(|r| r.borrow().values().cloned().collect());
    for h in handles {
        unsafe { paint_fence(HWND(h as *mut _)) };
    }
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
    let wc = WNDCLASSW {
        lpfnWndProc: Some(rename_wndproc),
        hInstance: hinstance,
        lpszClassName: RENAME_CLASS,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(
            (windows::Win32::Graphics::Gdi::COLOR_BTNFACE.0 + 1) as isize as *mut c_void,
        ),
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
        scroll_y: 0.0,
        hover: None,
        tracking_mouse: false,
        tooltip: 0,
        tooltip_text: Vec::new(),
        peeking: false,
        peek_armed: true,
        selected: Vec::new(),
        band: None,
        scrollbar_visible: false,
        displaced: None,
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

    // M13: content arrives via UpdateLayeredWindow (per-pixel alpha) on the
    // first paint — no SetLayeredWindowAttributes.
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

    // M13: bootstrap the layered window — until the first
    // UpdateLayeredWindow the window has no visible region and never
    // receives WM_PAINT, so the first frame must be pushed explicitly.
    paint_fence(hwnd);

    Ok(hwnd)
}

/// Ensure a renderer exists and push a frame (WM_PAINT body, reusable).
unsafe fn paint_fence(hwnd: HWND) {
    let Some(state) = state_mut(hwnd) else { return };
    if state.renderer.is_none() {
        match FenceRenderer::new(hwnd) {
            Ok(r) => state.renderer = Some(r),
            Err(e) => println!("renderer create failed: {e:?}"),
        }
    }
    // CRITICAL: UpdateLayeredWindow's psize RESIZES the window to the DIB.
    // If the surface ever lags the window (missed WM_SIZE ordering), a paint
    // would silently shrink the fence and sync_to_config would persist it.
    // Re-sync the surface to the client rect before every frame.
    if let Some(renderer) = state.renderer.as_mut() {
        let mut rc = RECT::default();
        if GetClientRect(hwnd, &mut rc).is_ok() {
            renderer.resize((rc.right - rc.left) as u32, (rc.bottom - rc.top) as u32);
        }
    }
    if let Some(renderer) = &state.renderer {
        let (items, icon_size) = fence_items(&state.id);
        let d = DROP_TARGET.with(|c| c.get());
        let drop = if d.0 == hwnd.0 as isize {
            if d.1 < 0 {
                render::DropIndicator::Ring
            } else {
                render::DropIndicator::Caret(d.1 as usize)
            }
        } else {
            render::DropIndicator::None
        };
        let chevron = if state.rolled_up {
            Some(if state.peeking { '\u{25BE}' } else { '\u{25B8}' })
        } else {
            None
        };
        let selected_idx: Vec<usize> = if state.selected.is_empty() {
            Vec::new()
        } else {
            items
                .iter()
                .enumerate()
                .filter(|(_, p)| state.selected.contains(*p))
                .map(|(i, _)| i)
                .collect()
        };
        let to_dip = 96.0 / GetDpiForWindow(hwnd) as f32;
        let band = state.band.map(|(a, b)| D2D_RECT_F {
            left: a.x.min(b.x) as f32 * to_dip,
            top: a.y.min(b.y) as f32 * to_dip,
            right: a.x.max(b.x) as f32 * to_dip,
            bottom: a.y.max(b.y) as f32 * to_dip,
        });
        let ok = renderer
            .draw(&render::DrawParams {
                title: &state.title,
                color: state.color,
                opacity: state.opacity,
                radius: state.corner_radius,
                items: &items,
                icon_size,
                hover: state.hover,
                scroll_y: state.scroll_y,
                chevron,
                drop,
                selected: &selected_idx,
                band,
                show_scrollbar: state.scrollbar_visible,
            })
            .is_ok();
        if !ok {
            // Device loss: drop the renderer; the next paint_fence call
            // rebuilds it (WM_PAINT invalidation doesn't reach ULW windows).
            state.renderer = None;
        }
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

/// Grid metrics for a fence window's current size and item count.
unsafe fn metrics_for(hwnd: HWND, id: &str) -> (render::GridMetrics, Vec<String>) {
    let to_dip = 96.0 / GetDpiForWindow(hwnd) as f32;
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let (items, icon_size) = fence_items(id);
    let m = render::grid_metrics(
        (rc.right - rc.left) as f32 * to_dip,
        (rc.bottom - rc.top) as f32 * to_dip,
        icon_size,
        items.len(),
    );
    (m, items)
}

/// Grid cell index under a client-space point (scroll-aware), or None
/// outside the grid.
unsafe fn icon_index_at(
    hwnd: HWND,
    id: &str,
    client_x: i32,
    client_y: i32,
    scroll_y: f32,
) -> Option<usize> {
    let to_dip = 96.0 / GetDpiForWindow(hwnd) as f32;
    // The grid lives in content space; the view is shifted up by scroll_y.
    if (client_y as f32 * to_dip) < render::TITLEBAR_HEIGHT {
        return None;
    }
    let (metrics, items) = metrics_for(hwnd, id);
    let i = metrics.index_at(
        client_x as f32 * to_dip,
        client_y as f32 * to_dip + scroll_y,
    )?;
    (i < items.len()).then_some(i)
}

/// Max scroll for a fence's current geometry and item count (DIPs).
unsafe fn max_scroll_for(hwnd: HWND, id: &str) -> f32 {
    let to_dip = 96.0 / GetDpiForWindow(hwnd) as f32;
    let mut rc = RECT::default();
    if GetClientRect(hwnd, &mut rc).is_err() {
        return 0.0;
    }
    let (items, icon_size) = fence_items(id);
    render::max_scroll(
        (rc.right - rc.left) as f32 * to_dip,
        (rc.bottom - rc.top) as f32 * to_dip,
        icon_size,
        items.len(),
    )
}

/// v1.2 (option 2): an unrolling/peeking fence pushes any ROLLED fences it
/// would cover down below its expanded rect, temporarily.
unsafe fn displace_rolled_under(coverer: HWND, coverer_id: &str, expanded_bottom: i32) {
    let mut crc = RECT::default();
    if GetWindowRect(coverer, &mut crc).is_err() {
        return;
    }
    let expanded = RECT {
        left: crc.left,
        top: crc.top,
        right: crc.right,
        bottom: expanded_bottom,
    };
    // Collect overlapped rolled fences, then stack them below the expansion
    // in their original vertical order (not all onto the same spot).
    let handles: Vec<isize> = REGISTRY.with(|r| r.borrow().values().cloned().collect());
    let mut hit: Vec<(isize, RECT)> = Vec::new();
    for h in handles {
        let other = HWND(h as *mut _);
        if other == coverer {
            continue;
        }
        let Some(ostate) = state_mut(other) else { continue };
        if !ostate.rolled_up || ostate.peeking || ostate.displaced.is_some() {
            continue;
        }
        let mut orc = RECT::default();
        if GetWindowRect(other, &mut orc).is_err() {
            continue;
        }
        let overlaps = orc.left < expanded.right
            && orc.right > expanded.left
            && orc.top < expanded.bottom
            && orc.bottom > expanded.top;
        if overlaps {
            hit.push((h, orc));
        }
    }
    hit.sort_by_key(|(_, orc)| orc.top);
    let mut cursor_y = expanded.bottom + SNAP_GAP;
    for (h, orc) in hit {
        let other = HWND(h as *mut _);
        if let Some(ostate) = state_mut(other) {
            ostate.displaced = Some((orc.top, coverer_id.to_string()));
        }
        let _ = SetWindowPos(
            other,
            HWND_BOTTOM,
            orc.left,
            cursor_y,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
        cursor_y += (orc.bottom - orc.top) + SNAP_GAP;
    }
}

/// Returns fences displaced by `coverer_id` to their original spots.
unsafe fn restore_displaced_by(coverer_id: &str) {
    let handles: Vec<isize> = REGISTRY.with(|r| r.borrow().values().cloned().collect());
    for h in handles {
        let hwnd = HWND(h as *mut _);
        let Some(state) = state_mut(hwnd) else { continue };
        if let Some((orig_y, by)) = &state.displaced {
            if by == coverer_id {
                let orig_y = *orig_y;
                state.displaced = None;
                let mut rc = RECT::default();
                let _ = GetWindowRect(hwnd, &mut rc);
                let _ = SetWindowPos(
                    hwnd,
                    HWND_BOTTOM,
                    rc.left,
                    orig_y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }
}

unsafe fn toggle_rollup(hwnd: HWND) {
    let mut rc = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rc);
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    let Some(state) = state_mut(hwnd) else { return };
    state.peeking = false;
    // Don't peek right back open under the cursor that just rolled us up.
    state.peek_armed = false;

    let id = state.id.clone();
    if state.rolled_up {
        state.rolled_up = false;
        let restore_h = state.restore_height;
        let _ = SetWindowPos(
            hwnd,
            HWND_BOTTOM,
            0,
            0,
            w,
            restore_h,
            SWP_NOMOVE | SWP_NOACTIVATE,
        );
        let mut rc = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rc);
        displace_rolled_under(hwnd, &id, rc.top + restore_h);
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
        restore_displaced_by(&id);
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
    // v1.2: a displaced fence sits at a temporary position — never persist.
    if state.displaced.is_some() {
        return;
    }
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

const SNAP_GAP: i32 = 24;

/// Other fences' window rects (excluding `hwnd`).
unsafe fn other_fence_rects(hwnd: HWND) -> Vec<RECT> {
    let mut out = Vec::new();
    REGISTRY.with(|r| {
        for &oh in r.borrow().values() {
            let other = HWND(oh as *mut _);
            if other == hwnd {
                continue;
            }
            let mut orc = RECT::default();
            if GetWindowRect(other, &mut orc).is_ok() {
                out.push(orc);
            }
        }
    });
    out
}

fn snap_best(cands: &[i32], cur: i32, threshold: i32) -> Option<i32> {
    cands
        .iter()
        .map(|&c| (c, (c - cur).abs()))
        .filter(|&(_, d)| d <= threshold)
        .min_by_key(|&(_, d)| d)
        .map(|(c, _)| c)
}

/// M11 (reworked): while the user drags a fence, snap its edges to OTHER
/// FENCES only — aligned edges or the standard 24px gap. Light grip.
unsafe fn snap_moving_rect(hwnd: HWND, rc: &mut RECT) {
    let threshold = (GetDpiForWindow(hwnd) as i32 * 8) / 96;
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    let mut xs: Vec<i32> = Vec::new();
    let mut ys: Vec<i32> = Vec::new();
    for orc in other_fence_rects(hwnd) {
        xs.extend_from_slice(&[
            orc.right + SNAP_GAP,    // sit right of it
            orc.left - w - SNAP_GAP, // sit left of it
            orc.left,                // left-align
            orc.right - w,           // right-align
        ]);
        ys.extend_from_slice(&[
            orc.bottom + SNAP_GAP,   // sit below it
            orc.top - h - SNAP_GAP,  // sit above it
            orc.top,                 // top-align
            orc.bottom - h,          // bottom-align
        ]);
    }
    if let Some(x) = snap_best(&xs, rc.left, threshold) {
        rc.left = x;
        rc.right = x + w;
    }
    if let Some(y) = snap_best(&ys, rc.top, threshold) {
        rc.top = y;
        rc.bottom = y + h;
    }
}

/// Resize snapping: the dragged edge(s) snap to other fences' edge lines
/// (aligned or gap-offset).
unsafe fn snap_sizing_rect(hwnd: HWND, edge: usize, rc: &mut RECT) {
    let threshold = (GetDpiForWindow(hwnd) as i32 * 8) / 96;
    let mut xs: Vec<i32> = Vec::new();
    let mut ys: Vec<i32> = Vec::new();
    for orc in other_fence_rects(hwnd) {
        xs.extend_from_slice(&[
            orc.left,
            orc.right,
            orc.left - SNAP_GAP,
            orc.right + SNAP_GAP,
        ]);
        ys.extend_from_slice(&[
            orc.top,
            orc.bottom,
            orc.top - SNAP_GAP,
            orc.bottom + SNAP_GAP,
        ]);
    }
    // WMSZ_*: LEFT 1, RIGHT 2, TOP 3, TOPLEFT 4, TOPRIGHT 5, BOTTOM 6,
    // BOTTOMLEFT 7, BOTTOMRIGHT 8.
    if matches!(edge, 1 | 4 | 7) {
        if let Some(x) = snap_best(&xs, rc.left, threshold) {
            rc.left = x;
        }
    }
    if matches!(edge, 2 | 5 | 8) {
        if let Some(x) = snap_best(&xs, rc.right, threshold) {
            rc.right = x;
        }
    }
    if matches!(edge, 3 | 4 | 5) {
        if let Some(y) = snap_best(&ys, rc.top, threshold) {
            rc.top = y;
        }
    }
    if matches!(edge, 6 | 7 | 8) {
        if let Some(y) = snap_best(&ys, rc.bottom, threshold) {
            rc.bottom = y;
        }
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

// ---- M10: hover tooltip (full name for truncated labels) ----

unsafe fn make_toolinfo(owner: HWND, text: *mut u16) -> TTTOOLINFOW {
    TTTOOLINFOW {
        // v5 comctl32 (no v6 manifest) rejects the modern struct size; the
        // V2 size stops after lParam, excluding lpReserved.
        cbSize: (std::mem::size_of::<TTTOOLINFOW>() - std::mem::size_of::<*mut c_void>()) as u32,
        uFlags: TTF_TRACK | TTF_ABSOLUTE,
        hwnd: owner,
        uId: 1,
        lpszText: PWSTR(text),
        ..Default::default()
    }
}

unsafe fn ensure_tooltip(hwnd: HWND, state: &mut FenceState) -> HWND {
    if state.tooltip != 0 {
        return HWND(state.tooltip as *mut _);
    }
    let hinstance: HINSTANCE = GetModuleHandleW(None).map(Into::into).unwrap_or_default();
    let tip = CreateWindowExW(
        WS_EX_TOPMOST,
        TOOLTIPS_CLASSW,
        PCWSTR::null(),
        WS_POPUP | WINDOW_STYLE(TTS_NOPREFIX | TTS_ALWAYSTIP),
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        hwnd,
        None,
        hinstance,
        None,
    )
    .unwrap_or_default();
    let mut empty = [0u16; 1];
    let ti = make_toolinfo(hwnd, empty.as_mut_ptr());
    SendMessageW(
        tip,
        TTM_ADDTOOLW,
        WPARAM(0),
        LPARAM(&ti as *const _ as isize),
    );
    SendMessageW(tip, TTM_SETMAXTIPWIDTH, WPARAM(0), LPARAM(500));
    state.tooltip = tip.0 as isize;
    tip
}

/// Shows the tracking tooltip near the cursor when the hovered item's label
/// is truncated; hides it otherwise.
unsafe fn update_tooltip(hwnd: HWND, state: &mut FenceState) {
    let full = state.hover.and_then(|i| {
        let (items, icon_size) = fence_items(&state.id);
        items
            .get(i)
            .and_then(|p| render::truncated_full_name(p, icon_size))
    });
    match full {
        Some(name) => {
            let tip = ensure_tooltip(hwnd, state);
            state.tooltip_text = name.encode_utf16().chain(std::iter::once(0)).collect();
            let ti = make_toolinfo(hwnd, state.tooltip_text.as_mut_ptr());
            SendMessageW(
                tip,
                TTM_UPDATETIPTEXTW,
                WPARAM(0),
                LPARAM(&ti as *const _ as isize),
            );
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let pos = (((pt.y + 24) & 0xFFFF) << 16) | ((pt.x + 12) & 0xFFFF);
            SendMessageW(tip, TTM_TRACKPOSITION, WPARAM(0), LPARAM(pos as isize));
            SendMessageW(
                tip,
                TTM_TRACKACTIVATE,
                WPARAM(1),
                LPARAM(&ti as *const _ as isize),
            );
        }
        None => {
            if state.tooltip != 0 {
                let ti = make_toolinfo(hwnd, std::ptr::null_mut());
                SendMessageW(
                    HWND(state.tooltip as *mut _),
                    TTM_TRACKACTIVATE,
                    WPARAM(0),
                    LPARAM(&ti as *const _ as isize),
                );
            }
        }
    }
}

/// Recomputes the hovered cell (e.g. after mouse move or scroll) and
/// repaints + retargets the tooltip when it changed.
unsafe fn update_hover(hwnd: HWND, client_x: i32, client_y: i32) {
    let Some(state) = state_mut(hwnd) else { return };
    let dragging = state.drag.as_ref().map(|d| d.active).unwrap_or(false);
    let idx = if dragging || (state.rolled_up && !state.peeking) {
        None
    } else {
        icon_index_at(hwnd, &state.id, client_x, client_y, state.scroll_y)
    };
    if state.hover != idx {
        state.hover = idx;
        update_tooltip(hwnd, state);
        paint_fence(hwnd);
    }
}

/// M13: grows the rubber-band to (x, y) and live-updates the selection to
/// every cell it touches.
unsafe fn update_band(hwnd: HWND, x: i32, y: i32) {
    let Some(state) = state_mut(hwnd) else { return };
    let Some(band) = &mut state.band else { return };
    band.1 = POINT { x, y };
    let (a, b) = *band;
    let to_dip = 96.0 / GetDpiForWindow(hwnd) as f32;
    let id = state.id.clone();
    let (metrics, items) = metrics_for(hwnd, &id);
    // Band in content-space DIPs (view + scroll).
    let (x0, x1) = (
        a.x.min(b.x) as f32 * to_dip,
        a.x.max(b.x) as f32 * to_dip,
    );
    let (y0, y1) = (
        a.y.min(b.y) as f32 * to_dip + state.scroll_y,
        a.y.max(b.y) as f32 * to_dip + state.scroll_y,
    );
    state.selected = items
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            let c = metrics.cell_rect(*i);
            c.left < x1 && c.right > x0 && c.top < y1 && c.bottom > y0
        })
        .map(|(_, p)| p.clone())
        .collect();
    paint_fence(hwnd);
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
    let (metrics, _) = metrics_for(target_hwnd, &target_id);
    let target_scroll = state_mut(target_hwnd).map(|s| s.scroll_y).unwrap_or(0.0);
    let drop_idx = metrics.index_at(
        cpt.x as f32 * to_dip,
        cpt.y as f32 * to_dip + target_scroll,
    );

    // M13: the drag carries every selected item; order is preserved.
    let changed = config::with(|cfg| {
        if src_id == target_id {
            let Some(f) = cfg.fences.iter_mut().find(|f| f.id == src_id) else {
                return false;
            };
            let dragged: Vec<String> = f
                .items
                .iter()
                .filter(|p| drag.paths.contains(p))
                .cloned()
                .collect();
            if dragged.is_empty() {
                return false;
            }
            let Some(di) = drop_idx else { return false };
            let to = di.min(f.items.len());
            let before = f.items[..to]
                .iter()
                .filter(|p| drag.paths.contains(p))
                .count();
            f.items.retain(|p| !drag.paths.contains(p));
            let insert_at = (to - before).min(f.items.len());
            for (k, it) in dragged.into_iter().enumerate() {
                f.items.insert(insert_at + k, it);
            }
            true
        } else {
            let Some(sf) = cfg.fences.iter_mut().find(|f| f.id == src_id) else {
                return false;
            };
            let dragged: Vec<String> = sf
                .items
                .iter()
                .filter(|p| drag.paths.contains(p))
                .cloned()
                .collect();
            if dragged.is_empty() {
                return false;
            }
            sf.items.retain(|p| !drag.paths.contains(p));
            let Some(tf) = cfg.fences.iter_mut().find(|f| f.id == target_id) else {
                return false;
            };
            match drop_idx {
                Some(di) if di < tf.items.len() => {
                    for (k, it) in dragged.into_iter().enumerate() {
                        tf.items.insert(di + k, it);
                    }
                }
                _ => tf.items.extend(dragged),
            }
            true
        }
    });
    if changed {
        config::schedule_save();
        paint_fence(src_hwnd);
        paint_fence(target_hwnd);
    }
}

/// Right-click title-bar menu: Rename, Color/Opacity/Corner radius submenus
/// (current value checked), Delete fence. Delete is disabled for an
/// "Unsorted" fence that still holds items (§5).
unsafe fn show_fence_menu(hwnd: HWND) {
    let Some(state) = state_mut(hwnd) else { return };
    let (deletable, color_str, opacity, radius) = config::with(|cfg| {
        cfg.fences
            .iter()
            .find(|f| f.id == state.id)
            .map(|f| {
                (
                    f.title != "Unsorted" || f.items.is_empty(),
                    f.color.clone(),
                    f.opacity,
                    f.corner_radius,
                )
            })
            .unwrap_or((false, String::new(), 0.78, 12.0))
    });
    let cur_hex = color_str
        .strip_prefix('#')
        .and_then(|v| u32::from_str_radix(v, 16).ok())
        .unwrap_or(0x1E1E2E);
    let cur_pct = (opacity * 100.0).round() as u32;
    let cur_radius = radius.round() as u32;

    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(menu, MF_STRING, IDM_FENCE_RENAME, w!("Rename"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_FENCE_AUTOORG, w!("Sorting rules…"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_FENCE_RAINBOW, w!("Sort by color"));

    let checked = |on: bool| if on { MF_CHECKED } else { MENU_ITEM_FLAGS(0) };
    if let Ok(color_menu) = CreatePopupMenu() {
        for (i, (name, hex)) in PALETTE.iter().enumerate() {
            let _ = AppendMenuW(
                color_menu,
                MF_STRING | checked(*hex == cur_hex),
                IDM_COLOR_BASE + i,
                *name,
            );
        }
        let _ = AppendMenuW(menu, MF_POPUP, color_menu.0 as usize, w!("Color"));
    }
    if let Ok(op_menu) = CreatePopupMenu() {
        for (i, (name, pct)) in OPACITIES.iter().enumerate() {
            let _ = AppendMenuW(
                op_menu,
                MF_STRING | checked(*pct == cur_pct),
                IDM_OPACITY_BASE + i,
                *name,
            );
        }
        let _ = AppendMenuW(menu, MF_POPUP, op_menu.0 as usize, w!("Opacity"));
    }
    if let Ok(rad_menu) = CreatePopupMenu() {
        for (i, (name, r)) in RADII.iter().enumerate() {
            let _ = AppendMenuW(
                rad_menu,
                MF_STRING | checked(*r == cur_radius),
                IDM_RADIUS_BASE + i,
                *name,
            );
        }
        let _ = AppendMenuW(menu, MF_POPUP, rad_menu.0 as usize, w!("Corner radius"));
    }
    let cur_icon = config::with(|c| c.icon_size);
    if let Ok(size_menu) = CreatePopupMenu() {
        for (i, (name, s)) in ICON_SIZES.iter().enumerate() {
            let _ = AppendMenuW(
                size_menu,
                MF_STRING | checked(*s == cur_icon),
                IDM_ICONSIZE_BASE + i,
                *name,
            );
        }
        let _ = AppendMenuW(menu, MF_POPUP, size_menu.0 as usize, w!("Icon size"));
    }

    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
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
    )
    .0 as usize;
    let _ = DestroyMenu(menu);

    match cmd {
        IDM_FENCE_DELETE => delete_fence(hwnd),
        IDM_FENCE_RENAME => open_rename_dialog(hwnd),
        IDM_FENCE_AUTOORG => crate::rules_ui::open(hwnd, &state.id),
        IDM_FENCE_RAINBOW => sort_fence_by_color(hwnd, &state.id.clone()),
        c if (IDM_COLOR_BASE..IDM_COLOR_BASE + PALETTE.len()).contains(&c) => {
            set_all_color(PALETTE[c - IDM_COLOR_BASE].1);
        }
        c if (IDM_OPACITY_BASE..IDM_OPACITY_BASE + OPACITIES.len()).contains(&c) => {
            set_all_opacity(OPACITIES[c - IDM_OPACITY_BASE].1);
        }
        c if (IDM_RADIUS_BASE..IDM_RADIUS_BASE + RADII.len()).contains(&c) => {
            set_all_radius(RADII[c - IDM_RADIUS_BASE].1);
        }
        c if (IDM_ICONSIZE_BASE..IDM_ICONSIZE_BASE + ICON_SIZES.len()).contains(&c) => {
            set_icon_size(hwnd, ICON_SIZES[c - IDM_ICONSIZE_BASE].1);
        }
        _ => {}
    }
}

/// M11: global icon size — re-extract every icon at the new pixel size and
/// rebuild all per-target bitmap caches.
pub(crate) unsafe fn set_icon_size(hwnd: HWND, size: u32) {
    config::with(|c| c.icon_size = size);
    config::schedule_save();
    icons::clear_cache();
    let px = size * GetDpiForWindow(hwnd) / 96;
    let all: Vec<String> =
        config::with(|c| c.fences.iter().flat_map(|f| f.items.iter().cloned()).collect());
    icons::preload(&all, px);
    reset_renderers();
    invalidate_all();
}

/// Clears icon selection in every fence (blank-space click anywhere).
pub fn clear_all_selections() {
    unsafe {
        for_each_state_pub();
    }
}

unsafe fn for_each_state_pub() {
    let handles: Vec<isize> = REGISTRY.with(|r| r.borrow().values().cloned().collect());
    for h in handles {
        let hwnd = HWND(h as *mut _);
        if let Some(state) = state_mut(hwnd) {
            if !state.selected.is_empty() {
                state.selected.clear();
                paint_fence(hwnd);
            }
        }
    }
}

/// Applies a mutation to every live fence's window state.
unsafe fn for_each_state(mut f: impl FnMut(&mut FenceState)) {
    let handles: Vec<isize> = REGISTRY.with(|r| r.borrow().values().cloned().collect());
    for h in handles {
        if let Some(state) = state_mut(HWND(h as *mut _)) {
            f(state);
        }
    }
}

// Appearance settings apply universally to every fence (user preference).
pub(crate) unsafe fn set_all_color(hex: u32) {
    config::with(|cfg| {
        for f in &mut cfg.fences {
            f.color = format!("#{hex:06X}");
        }
    });
    for_each_state(|s| s.color = color_from_hex(hex));
    config::schedule_save();
    invalidate_all();
}

pub(crate) unsafe fn set_all_opacity(pct: u32) {
    config::with(|cfg| {
        for f in &mut cfg.fences {
            f.opacity = pct as f32 / 100.0;
        }
    });
    for_each_state(|s| s.opacity = pct as f32 / 100.0);
    config::schedule_save();
    invalidate_all();
}

pub(crate) unsafe fn set_all_radius(r: u32) {
    config::with(|cfg| {
        for f in &mut cfg.fences {
            f.corner_radius = r as f32;
        }
    });
    for_each_state(|s| s.corner_radius = r as f32);
    config::schedule_save();
    invalidate_all();
}

/// v1.2 fun: reorder a fence's items in rainbow hue order; near-grays go
/// last, brightest first.
unsafe fn sort_fence_by_color(hwnd: HWND, id: &str) {
    config::with(|cfg| {
        if let Some(f) = cfg.fences.iter_mut().find(|f| f.id == id) {
            f.items.sort_by_cached_key(|p| icons::rainbow_key(p));
        }
    });
    config::schedule_save();
    paint_fence(hwnd);
}

// ---- Rename dialog (§7 v1 fallback: tiny modal window with an EDIT) ----

/// M10: lets main.rs open the rename dialog right after creating a fence.
pub unsafe fn begin_rename(hwnd: HWND) {
    open_rename_dialog(hwnd);
}

struct RenameCtx {
    fence_id: String,
    fence_hwnd: isize,
    edit: isize,
    font: isize,
}

unsafe fn open_rename_dialog(fence_hwnd: HWND) {
    // One at a time; refocus an existing dialog instead of stacking.
    let existing = RENAME_HWND.with(|c| c.get());
    if existing != 0 {
        let _ = SetForegroundWindow(HWND(existing as *mut _));
        return;
    }
    let Some(state) = state_mut(fence_hwnd) else { return };
    let Ok(hmodule) = GetModuleHandleW(None) else { return };
    let hinstance: HINSTANCE = hmodule.into();

    let mut frc = RECT::default();
    let _ = GetWindowRect(fence_hwnd, &mut frc);
    let (dw, dh) = (280, 130);
    let hmon = MonitorFromWindow(fence_hwnd, MONITOR_DEFAULTTONEAREST);
    let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO {
        cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
        ..Default::default()
    };
    let _ = windows::Win32::Graphics::Gdi::GetMonitorInfoW(hmon, &mut mi);
    let dx = ((frc.left + frc.right) / 2 - dw / 2)
        .clamp(mi.rcWork.left, (mi.rcWork.right - dw).max(mi.rcWork.left));
    let dy = ((frc.top + frc.bottom) / 2 - dh / 2)
        .clamp(mi.rcWork.top, (mi.rcWork.bottom - dh).max(mi.rcWork.top));
    let dlg = match CreateWindowExW(
        WS_EX_TOOLWINDOW,
        RENAME_CLASS,
        w!("Rename fence"),
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

    let font = windows::Win32::Graphics::Gdi::CreateFontW(
        -15,
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        windows::Win32::Graphics::Gdi::DEFAULT_CHARSET.0 as u32,
        windows::Win32::Graphics::Gdi::OUT_DEFAULT_PRECIS.0 as u32,
        windows::Win32::Graphics::Gdi::CLIP_DEFAULT_PRECIS.0 as u32,
        windows::Win32::Graphics::Gdi::CLEARTYPE_QUALITY.0 as u32,
        0, // default pitch and family
        w!("Segoe UI"),
    );

    let mut title_utf16: Vec<u16> = state.title.encode_utf16().collect();
    title_utf16.push(0);
    let edit = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("EDIT"),
        PCWSTR(title_utf16.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
        10,
        12,
        244,
        24,
        dlg,
        HMENU(100 as *mut c_void),
        hinstance,
        None,
    )
    .unwrap_or_default();
    let ok = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("BUTTON"),
        w!("OK"),
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
        98,
        52,
        75,
        26,
        dlg,
        HMENU(1 as *mut c_void), // IDOK
        hinstance,
        None,
    )
    .unwrap_or_default();
    let cancel = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("BUTTON"),
        w!("Cancel"),
        WS_CHILD | WS_VISIBLE,
        179,
        52,
        75,
        26,
        dlg,
        HMENU(2 as *mut c_void), // IDCANCEL
        hinstance,
        None,
    )
    .unwrap_or_default();
    for child in [edit, ok, cancel] {
        SendMessageW(child, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }

    let ctx = Box::new(RenameCtx {
        fence_id: state.id.clone(),
        fence_hwnd: fence_hwnd.0 as isize,
        edit: edit.0 as isize,
        font: font.0 as isize,
    });
    SetWindowLongPtrW(dlg, GWLP_USERDATA, Box::into_raw(ctx) as isize);
    RENAME_HWND.with(|c| c.set(dlg.0 as isize));

    let _ = ShowWindow(dlg, SW_SHOW);
    let _ = SetForegroundWindow(dlg);
    let _ = SetFocus(edit);
    SendMessageW(edit, EM_SETSEL, WPARAM(0), LPARAM(-1)); // select all
}

unsafe fn commit_rename(dlg: HWND) {
    let ctx = GetWindowLongPtrW(dlg, GWLP_USERDATA) as *mut RenameCtx;
    let Some(ctx) = ctx.as_ref() else { return };
    let mut buf = [0u16; 256];
    let len = GetWindowTextW(HWND(ctx.edit as *mut _), &mut buf);
    let title = String::from_utf16_lossy(&buf[..len as usize])
        .trim()
        .to_string();
    if !title.is_empty() {
        let fence_hwnd = HWND(ctx.fence_hwnd as *mut _);
        config::with(|cfg| {
            if let Some(f) = cfg.fences.iter_mut().find(|f| f.id == ctx.fence_id) {
                f.title = title.clone();
            }
        });
        if let Some(state) = state_mut(fence_hwnd) {
            state.title = title.clone();
        }
        let mut wide: Vec<u16> = title.encode_utf16().collect();
        wide.push(0);
        let _ = SetWindowTextW(fence_hwnd, PCWSTR(wide.as_ptr()));
        paint_fence(fence_hwnd);
        config::schedule_save();
    }
}

extern "system" fn rename_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                match wparam.0 & 0xFFFF {
                    1 => {
                        // IDOK (default button; Enter lands here via
                        // IsDialogMessageW in the main loop)
                        commit_rename(hwnd);
                        let _ = DestroyWindow(hwnd);
                    }
                    2 => {
                        // IDCANCEL (Cancel button or Esc)
                        let _ = DestroyWindow(hwnd);
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
                RENAME_HWND.with(|c| c.set(0));
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RenameCtx;
                if !ptr.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let ctx = Box::from_raw(ptr);
                    let _ = windows::Win32::Graphics::Gdi::DeleteObject(
                        windows::Win32::Graphics::Gdi::HFONT(ctx.font as *mut _),
                    );
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn delete_fence(hwnd: HWND) {
    let Some(state) = state_mut(hwnd) else { return };
    let id = state.id.clone();
    restore_displaced_by(&id);
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
                paint_fence(h);
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
            // resize behavior but no visible frame. Handle BOTH wparam
            // variants — creation sends FALSE, and falling through to
            // DefWindowProc there yields a 16px-smaller client, which ULW's
            // psize would then bake into the window size.
            WM_NCCALCSIZE => LRESULT(0),
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
                    // Clicking the chevron (right end of a rolled fence's
                    // title bar) toggles the roll-up directly.
                    let mut rc = RECT::default();
                    let _ = GetClientRect(hwnd, &mut rc);
                    let chevron_zone =
                        rc.right - (GetDpiForWindow(hwnd) as i32 * 26) / 96;
                    let rolled = state_mut(hwnd).map(|s| s.rolled_up).unwrap_or(false);
                    if rolled && x >= chevron_zone {
                        toggle_rollup(hwnd);
                    } else {
                        // Grabbing a displaced fence adopts its current spot
                        // as the real one.
                        if let Some(state) = state_mut(hwnd) {
                            state.displaced = None;
                        }
                        // Title-bar drag moves the fence via the system move
                        // loop.
                        let _ = ReleaseCapture();
                        SendMessageW(
                            hwnd,
                            WM_NCLBUTTONDOWN,
                            WPARAM(HTCAPTION as usize),
                            LPARAM(0),
                        );
                    }
                } else if let Some(state) = state_mut(hwnd) {
                    let ctrl = (wparam.0 & 0x0008) != 0; // MK_CONTROL
                    if let Some(i) = icon_index_at(hwnd, &state.id, x, y, state.scroll_y) {
                        let (items, _) = fence_items(&state.id);
                        if let Some(item) = items.get(i) {
                            if ctrl {
                                // M13: Ctrl+click toggles membership.
                                match state.selected.iter().position(|p| p == item) {
                                    Some(pos) => {
                                        state.selected.remove(pos);
                                    }
                                    None => state.selected.push(item.clone()),
                                }
                                paint_fence(hwnd);
                            } else {
                                // Plain press on an unselected icon collapses
                                // the selection to it; on a selected icon it
                                // keeps the group (so the drag moves all).
                                if !state.selected.contains(item) {
                                    state.selected = vec![item.clone()];
                                    paint_fence(hwnd);
                                }
                                // Candidate drag: activates past the system
                                // threshold, so double-click launch still works.
                                state.drag = Some(DragState {
                                    paths: state.selected.clone(),
                                    start: POINT { x, y },
                                    active: false,
                                    cursor: None,
                                });
                                SetCapture(hwnd);
                            }
                        }
                    } else {
                        // M13: empty space clears selection everywhere and
                        // begins a rubber-band selection here.
                        if !ctrl {
                            clear_all_selections();
                        }
                        state.band = Some((POINT { x, y }, POINT { x, y }));
                        SetCapture(hwnd);
                    }
                }
                LRESULT(0)
            }
            // M10: mouse wheel scrolls the icon grid (reaches unactivated
            // windows via Win10+'s scroll-inactive-windows-on-hover).
            WM_MOUSEWHEEL => {
                if let Some(state) = state_mut(hwnd) {
                    if !state.rolled_up || state.peeking {
                        let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
                        let max = max_scroll_for(hwnd, &state.id);
                        let new = (state.scroll_y - delta / 120.0 * 48.0).clamp(0.0, max);
                        if new != state.scroll_y {
                            state.scroll_y = new;
                            // v1.2: scrollbar visible while scrolling; a
                            // timer hides it again.
                            state.scrollbar_visible = true;
                            SetTimer(hwnd, TIMER_SCROLLBAR, 1200, None);
                            paint_fence(hwnd);
                            // Cells moved under the cursor: refresh hover.
                            let mut pt = POINT::default();
                            let _ = GetCursorPos(&mut pt);
                            let _ = ScreenToClient(hwnd, &mut pt);
                            update_hover(hwnd, pt.x, pt.y);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_SCROLLBAR => {
                let _ = KillTimer(hwnd, TIMER_SCROLLBAR);
                if let Some(state) = state_mut(hwnd) {
                    if state.scrollbar_visible {
                        state.scrollbar_visible = false;
                        paint_fence(hwnd);
                    }
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                if let Some(state) = state_mut(hwnd) {
                    state.tracking_mouse = false;
                    state.peek_armed = true;
                    if state.hover.is_some() {
                        state.hover = None;
                        update_tooltip(hwnd, state);
                        paint_fence(hwnd);
                    }
                    // M11: collapse the peek once the cursor leaves.
                    if state.peeking {
                        state.peeking = false;
                        let peek_id = state.id.clone();
                        let mut rc = RECT::default();
                        let _ = GetWindowRect(hwnd, &mut rc);
                        let _ = SetWindowPos(
                            hwnd,
                            HWND_BOTTOM,
                            0,
                            0,
                            rc.right - rc.left,
                            titlebar_height_px(hwnd),
                            SWP_NOMOVE | SWP_NOACTIVATE,
                        );
                        restore_displaced_by(&peek_id);
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if let Some(state) = state_mut(hwnd) {
                    // M13: rubber-band update.
                    if state.band.is_some() {
                        update_band(hwnd, x, y);
                        return LRESULT(0);
                    }
                    let mut drag_active = false;
                    if let Some(drag) = &mut state.drag {
                        if !drag.active
                            && ((x - drag.start.x).abs() > GetSystemMetrics(SM_CXDRAG)
                                || (y - drag.start.y).abs() > GetSystemMetrics(SM_CYDRAG))
                        {
                            drag.active = true;
                            drag.cursor = icons::drag_cursor(&drag.paths[0]);
                        }
                        if drag.active {
                            drag_active = true;
                            if let Some(cur) = drag.cursor {
                                SetCursor(cur);
                            }
                        }
                    }
                    // M11: live drop indicator while dragging.
                    if drag_active {
                        let src_id = state.id.clone();
                        let mut spt = POINT { x, y };
                        let _ = ClientToScreen(hwnd, &mut spt);
                        let target = fence_at_point(spt).map(|(tid, th)| {
                            if tid == src_id {
                                // Same fence: insertion caret index.
                                let ts = state_mut(th).map(|s| s.scroll_y).unwrap_or(0.0);
                                let to_dip = 96.0 / GetDpiForWindow(th) as f32;
                                let (metrics, items) = metrics_for(th, &tid);
                                let mut cpt = spt;
                                let _ = ScreenToClient(th, &mut cpt);
                                let idx = metrics
                                    .index_at(cpt.x as f32 * to_dip, cpt.y as f32 * to_dip + ts)
                                    .unwrap_or(items.len())
                                    .min(items.len());
                                (th, idx as i32)
                            } else {
                                (th, -1)
                            }
                        });
                        set_drop_target(target);
                    }
                    // M11: rolled fence peeks open under the cursor (only
                    // when armed by a previous mouse-leave).
                    if state.rolled_up
                        && !state.peeking
                        && state.peek_armed
                        && state.drag.is_none()
                    {
                        state.peeking = true;
                        state.peek_armed = false;
                        let peek_id = state.id.clone();
                        let restore_h = state.restore_height;
                        let mut rc = RECT::default();
                        let _ = GetWindowRect(hwnd, &mut rc);
                        let _ = SetWindowPos(
                            hwnd,
                            HWND_BOTTOM,
                            0,
                            0,
                            rc.right - rc.left,
                            restore_h,
                            SWP_NOMOVE | SWP_NOACTIVATE,
                        );
                        displace_rolled_under(hwnd, &peek_id, rc.top + restore_h);
                    }
                    if !state.tracking_mouse {
                        state.tracking_mouse = true;
                        let _ = TrackMouseEvent(&mut TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        });
                    }
                }
                update_hover(hwnd, x, y);
                LRESULT(0)
            }
            // M11 (reworked): snap to neighboring fences while moving or
            // resizing.
            WM_MOVING => {
                let rc = lparam.0 as *mut RECT;
                snap_moving_rect(hwnd, &mut *rc);
                LRESULT(1)
            }
            WM_SIZING => {
                let rc = lparam.0 as *mut RECT;
                snap_sizing_rect(hwnd, wparam.0, &mut *rc);
                LRESULT(1)
            }
            WM_LBUTTONUP => {
                // M13: finish a rubber-band (selection already applied live).
                if let Some(state) = state_mut(hwnd) {
                    if state.band.take().is_some() {
                        let _ = ReleaseCapture();
                        paint_fence(hwnd);
                        return LRESULT(0);
                    }
                }
                let taken = state_mut(hwnd).and_then(|s| s.drag.take());
                if let Some(drag) = taken {
                    let _ = ReleaseCapture();
                    set_drop_target(None);
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
                        set_drop_target(None);
                    }
                    if state.band.take().is_some() {
                        paint_fence(hwnd);
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
                    if let Some(i) = icon_index_at(hwnd, &state.id, x, y, state.scroll_y) {
                        let (items, _) = fence_items(&state.id);
                        if let Some(item) = items.get(i) {
                            crate::launch::launch(item);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if y < titlebar_height_px(hwnd) {
                    show_fence_menu(hwnd);
                } else if let Some(state) = state_mut(hwnd) {
                    // M13: real Explorer context menu for the item under the
                    // cursor.
                    if let Some(i) = icon_index_at(hwnd, &state.id, x, y, state.scroll_y) {
                        let (items, _) = fence_items(&state.id);
                        if let Some(item) = items.get(i) {
                            let mut pt = POINT { x, y };
                            let _ = ClientToScreen(hwnd, &mut pt);
                            crate::shellmenu::show(hwnd, item, pt);
                        }
                    }
                }
                LRESULT(0)
            }
            // M13: shell submenus (Open with, Send to) populate via these.
            WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM => {
                if crate::shellmenu::handle_menu_msg(msg, wparam, lparam) {
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
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
                paint_fence(hwnd);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_SIZE => {
                if let Some(state) = state_mut(hwnd) {
                    if let Some(renderer) = state.renderer.as_mut() {
                        renderer.resize(
                            (lparam.0 & 0xFFFF) as u32,
                            ((lparam.0 >> 16) & 0xFFFF) as u32,
                        );
                    }
                }
                // ULW windows get no WM_PAINT: push the frame now.
                paint_fence(hwnd);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                // Rebuild the renderer at the new DPI on the next paint.
                if let Some(state) = state_mut(hwnd) {
                    state.renderer = None;
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
