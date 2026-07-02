// Direct2D / DirectWrite rendering for fence windows.
// One process-wide D2D factory + one DirectWrite factory (UI thread only);
// per-fence, an ID2D1HwndRenderTarget wrapped in FenceRenderer.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP,
    HDC, HGDIOBJ,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, UpdateLayeredWindow, ULW_ALPHA};

use crate::icons;

pub const TITLEBAR_HEIGHT: f32 = 28.0; // DIPs

/// M11: what to show for an in-progress icon drag over this fence.
pub enum DropIndicator {
    None,
    /// Cross-fence reassign target: highlight ring around the fence.
    Ring,
    /// Same-fence reorder: insertion caret before this cell index.
    Caret(usize),
}

/// Everything `FenceRenderer::draw` needs for one frame.
pub struct DrawParams<'a> {
    pub title: &'a str,
    pub color: D2D1_COLOR_F,
    pub opacity: f32,
    pub radius: f32,
    pub items: &'a [String],
    pub icon_size: f32,
    pub hover: Option<usize>,
    pub scroll_y: f32,
    /// Title-bar glyph for rolled state ('▸' rolled, '▾' peeking).
    pub chevron: Option<char>,
    pub drop: DropIndicator,
    /// M13: selected cell indices (stronger highlight).
    pub selected: &'a [usize],
    /// M13: rubber-band rectangle in view-space DIPs, while band-selecting.
    pub band: Option<D2D_RECT_F>,
    /// v1.2: scrollbar appears only while scrolling.
    pub show_scrollbar: bool,
}

/// Icon grid geometry, all in DIPs. Shared by drawing and hit-testing so a
/// double-click always maps to the cell the user sees. v1.2: the used
/// columns are centered horizontally, and when everything fits without
/// scrolling the rows are centered vertically too.
pub struct GridMetrics {
    pub cell_w: f32,
    pub cell_h: f32,
    pub cols: usize,
    pub origin_x: f32,
    pub origin_y: f32,
    pub content_h: f32,
    pub max_scroll: f32,
}

pub fn grid_metrics(
    width_dips: f32,
    height_dips: f32,
    icon_size: f32,
    count: usize,
) -> GridMetrics {
    let cell_w = icon_size + 28.0;
    let cell_h = icon_size + 22.0;
    let cols = (((width_dips - 20.0) / cell_w).floor() as usize).max(1);
    let rows = if count == 0 { 0 } else { (count - 1) / cols + 1 };
    // Center by the columns actually used (a 2-item fence centers 2 cells).
    let eff_cols = cols.min(count.max(1));
    let origin_x = ((width_dips - eff_cols as f32 * cell_w) / 2.0).max(10.0);
    let body_top = TITLEBAR_HEIGHT + 8.0;
    let rows_h = rows as f32 * cell_h;
    let avail = (height_dips - body_top - 8.0).max(0.0);
    let origin_y = if rows_h <= avail {
        body_top + (avail - rows_h) / 2.0
    } else {
        body_top
    };
    let content_h = body_top + rows_h + 8.0;
    GridMetrics {
        cell_w,
        cell_h,
        cols,
        origin_x,
        origin_y,
        content_h,
        max_scroll: (content_h - height_dips).max(0.0),
    }
}

impl GridMetrics {
    pub fn cell_rect(&self, i: usize) -> D2D_RECT_F {
        let row = i / self.cols;
        let col = i % self.cols;
        let left = self.origin_x + col as f32 * self.cell_w;
        let top = self.origin_y + row as f32 * self.cell_h;
        D2D_RECT_F {
            left,
            top,
            right: left + self.cell_w,
            bottom: top + self.cell_h,
        }
    }

    pub fn index_at(&self, x: f32, y: f32) -> Option<usize> {
        if y < self.origin_y || x < self.origin_x {
            return None;
        }
        let col = ((x - self.origin_x) / self.cell_w) as usize;
        if col >= self.cols {
            return None;
        }
        let row = ((y - self.origin_y) / self.cell_h) as usize;
        Some(row * self.cols + col)
    }
}

/// Maximum scroll offset for a fence of this size and item count.
pub fn max_scroll(width_dips: f32, height_dips: f32, icon_size: f32, count: usize) -> f32 {
    grid_metrics(width_dips, height_dips, icon_size, count).max_scroll
}

/// Full display name if the label at this cell width gets truncated (i.e. a
/// tooltip is worth showing); None when the label already fits.
pub fn truncated_full_name(path: &str, icon_size: f32) -> Option<String> {
    let cell_w = icon_size + 28.0;
    let max_chars = (cell_w / 6.0) as usize;
    let full = display_name(path);
    if middle_truncate(&full, max_chars) == full {
        None
    } else {
        Some(full)
    }
}

/// What the shell shows: .lnk/.url hide their extension, everything else
/// keeps its full file name.
fn display_name(path: &str) -> String {
    let p = Path::new(path);
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let name = match ext.as_deref() {
        Some("lnk") | Some("url") => p.file_stem(),
        _ => p.file_name(),
    };
    name.and_then(|n| n.to_str()).unwrap_or(path).to_string()
}

fn middle_truncate(name: &str, max_chars: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max_chars || max_chars < 3 {
        return name.to_string();
    }
    let keep = max_chars - 1;
    let front = keep / 2 + keep % 2;
    let back = keep / 2;
    let head: String = chars[..front].iter().collect();
    let tail: String = chars[chars.len() - back..].iter().collect();
    format!("{head}\u{2026}{tail}")
}

thread_local! {
    static D2D_FACTORY: OnceCell<ID2D1Factory> = const { OnceCell::new() };
    static DWRITE_FACTORY: OnceCell<IDWriteFactory> = const { OnceCell::new() };
}

fn d2d_factory() -> Result<ID2D1Factory> {
    D2D_FACTORY.with(|c| {
        if let Some(f) = c.get() {
            return Ok(f.clone());
        }
        let f: ID2D1Factory =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let _ = c.set(f.clone());
        Ok(f)
    })
}

fn dwrite_factory() -> Result<IDWriteFactory> {
    DWRITE_FACTORY.with(|c| {
        if let Some(f) = c.get() {
            return Ok(f.clone());
        }
        let f: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let _ = c.set(f.clone());
        Ok(f)
    })
}

pub struct FenceRenderer {
    // M13: per-pixel alpha — a DC render target draws into a premultiplied
    // 32bpp DIB, presented with UpdateLayeredWindow. The fence background is
    // genuinely translucent against the wallpaper; icons/text stay opaque.
    target: ID2D1DCRenderTarget,
    hwnd_raw: isize,
    memdc: HDC,
    dib: HBITMAP,
    old_bitmap: HGDIOBJ,
    width_px: u32,
    height_px: u32,
    title_format: IDWriteTextFormat,
    label_format: IDWriteTextFormat,
    hint_format: IDWriteTextFormat,
    // ID2D1Bitmaps are per-render-target resources; each renderer creates its
    // own from the shared CPU-side pixel cache in icons.rs. None = failed,
    // don't retry every paint.
    bitmaps: RefCell<HashMap<String, Option<ID2D1Bitmap>>>,
}

impl Drop for FenceRenderer {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.memdc, self.old_bitmap);
            let _ = DeleteObject(self.dib);
            let _ = DeleteDC(self.memdc);
        }
    }
}

/// 32bpp top-down premultiplied DIB selected into a memory DC.
unsafe fn make_surface(w: u32, h: u32) -> Result<(HDC, HBITMAP, HGDIOBJ)> {
    let memdc = CreateCompatibleDC(None);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let dib = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
    let old = SelectObject(memdc, dib);
    Ok((memdc, dib, old))
}

impl FenceRenderer {
    pub fn new(hwnd: HWND) -> Result<Self> {
        unsafe {
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc)?;
            let width_px = (rc.right - rc.left).max(1) as u32;
            let height_px = (rc.bottom - rc.top).max(1) as u32;

            let dpi = GetDpiForWindow(hwnd) as f32;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: dpi,
                dpiY: dpi,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let target = d2d_factory()?.CreateDCRenderTarget(&props)?;
            let (memdc, dib, old_bitmap) = make_surface(width_px, height_px)?;
            target.BindDC(
                memdc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: width_px as i32,
                    bottom: height_px as i32,
                },
            )?;

            let title_format = dwrite_factory()?.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                13.0,
                w!("en-us"),
            )?;
            title_format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            title_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?; // v1.2
            title_format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;

            let label_format = dwrite_factory()?.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                11.0,
                w!("en-us"),
            )?;
            label_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            label_format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;

            let hint_format = dwrite_factory()?.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                13.0,
                w!("en-us"),
            )?;
            hint_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            hint_format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            Ok(Self {
                target,
                hwnd_raw: hwnd.0 as isize,
                memdc,
                dib,
                old_bitmap,
                width_px,
                height_px,
                title_format,
                label_format,
                hint_format,
                bitmaps: RefCell::new(HashMap::new()),
            })
        }
    }

    pub fn resize(&mut self, width_px: u32, height_px: u32) {
        unsafe {
            let (w, h) = (width_px.max(1), height_px.max(1));
            if (w, h) == (self.width_px, self.height_px) {
                return;
            }
            if let Ok((memdc, dib, old)) = make_surface(w, h) {
                SelectObject(self.memdc, self.old_bitmap);
                let _ = DeleteObject(self.dib);
                let _ = DeleteDC(self.memdc);
                self.memdc = memdc;
                self.dib = dib;
                self.old_bitmap = old;
                self.width_px = w;
                self.height_px = h;
                let _ = self.target.BindDC(
                    self.memdc,
                    &RECT {
                        left: 0,
                        top: 0,
                        right: w as i32,
                        bottom: h as i32,
                    },
                );
            }
        }
    }

    /// Push the rendered DIB to the layered window (per-pixel alpha).
    unsafe fn present(&self) {
        let size = SIZE {
            cx: self.width_px as i32,
            cy: self.height_px as i32,
        };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let r = UpdateLayeredWindow(
            HWND(self.hwnd_raw as *mut _),
            None,
            None,
            Some(&size),
            self.memdc,
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        if let Err(e) = r {
            println!("UpdateLayeredWindow failed: {e:?}");
        }
    }

    fn icon_bitmap(&self, path: &str) -> Option<ID2D1Bitmap> {
        let mut cache = self.bitmaps.borrow_mut();
        if let Some(entry) = cache.get(path) {
            return entry.clone();
        }
        let bmp = icons::with_pixels(path, |px| unsafe {
            self.target
                .CreateBitmap(
                    D2D_SIZE_U {
                        width: px.size,
                        height: px.size,
                    },
                    Some(px.bgra.as_ptr() as *const c_void),
                    px.size * 4,
                    &D2D1_BITMAP_PROPERTIES {
                        pixelFormat: D2D1_PIXEL_FORMAT {
                            format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                        },
                        dpiX: 96.0,
                        dpiY: 96.0,
                    },
                )
                .ok()
        })
        .flatten();
        cache.insert(path.to_string(), bmp.clone());
        bmp
    }

    /// Draws the fence chrome (rounded-rect background with fence opacity in
    /// the brush alpha, title bar strip + optional chevron, title text), the
    /// icon grid (scrolled, hover-highlighted, scrollbar on overflow), the
    /// empty-fence hint, and any drag drop indicator.
    pub fn draw(&self, p: &DrawParams) -> Result<()> {
        let (title, color, opacity, radius, items, icon_size, hover, scroll_y) = (
            p.title,
            p.color,
            p.opacity,
            p.radius,
            p.items,
            p.icon_size,
            p.hover,
            p.scroll_y,
        );
        unsafe {
            let t = &self.target;
            t.BeginDraw();
            // Fully transparent base: corners outside the rounded rect stay
            // invisible, and the background fill's alpha is real translucency.
            t.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));

            let size = t.GetSize();
            let rr = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: size.width,
                    bottom: size.height,
                },
                radiusX: radius,
                radiusY: radius,
            };

            let bg = t.CreateSolidColorBrush(&D2D1_COLOR_F { a: opacity, ..color }, None)?;
            t.FillRoundedRectangle(&rr, &bg);

            let strip = t.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 0.08,
                },
                None,
            )?;
            t.PushAxisAlignedClip(
                &D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: size.width,
                    bottom: TITLEBAR_HEIGHT,
                },
                D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            );
            t.FillRoundedRectangle(&rr, &strip);
            t.PopAxisAlignedClip();

            let text_brush = t.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 0.92,
                },
                None,
            )?;
            let title_utf16: Vec<u16> = title.encode_utf16().collect();
            t.DrawText(
                &title_utf16,
                &self.title_format,
                &D2D_RECT_F {
                    left: 10.0,
                    top: 0.0,
                    right: (size.width - 10.0).max(10.0),
                    bottom: TITLEBAR_HEIGHT,
                },
                &text_brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );

            // M11: rolled-state chevron at the right end of the title bar.
            if let Some(ch) = p.chevron {
                let mut buf = [0u16; 2];
                let encoded = ch.encode_utf16(&mut buf);
                t.DrawText(
                    &*encoded,
                    &self.title_format,
                    &D2D_RECT_F {
                        left: size.width - 22.0,
                        top: 0.0,
                        right: size.width - 6.0,
                        bottom: TITLEBAR_HEIGHT,
                    },
                    &text_brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }

            // Icon grid: bitmap centered in each cell, label below,
            // middle-truncated to the cell width. Cells live in content
            // space and are shifted up by the scroll offset, clipped to the
            // area below the title bar.
            let metrics = grid_metrics(size.width, size.height, icon_size, items.len());
            let max_chars = (metrics.cell_w / 6.0) as usize;
            let content_h = metrics.content_h;
            let max_scroll = metrics.max_scroll;
            let scroll = scroll_y.clamp(0.0, max_scroll);

            t.PushAxisAlignedClip(
                &D2D_RECT_F {
                    left: 0.0,
                    top: TITLEBAR_HEIGHT,
                    right: size.width,
                    bottom: size.height,
                },
                D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            );
            for (i, item) in items.iter().enumerate() {
                let mut cell = metrics.cell_rect(i);
                cell.top -= scroll;
                cell.bottom -= scroll;
                if cell.bottom < TITLEBAR_HEIGHT {
                    continue;
                }
                if cell.top > size.height {
                    break;
                }
                let is_selected = p.selected.contains(&i);
                if hover == Some(i) || is_selected {
                    let hl = t.CreateSolidColorBrush(
                        &D2D1_COLOR_F {
                            r: 1.0,
                            g: 1.0,
                            b: 1.0,
                            a: if is_selected { 0.16 } else { 0.10 },
                        },
                        None,
                    )?;
                    let cell_rr = D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F {
                            left: cell.left + 2.0,
                            top: cell.top,
                            right: cell.right - 2.0,
                            bottom: cell.bottom - 2.0,
                        },
                        radiusX: 6.0,
                        radiusY: 6.0,
                    };
                    t.FillRoundedRectangle(&cell_rr, &hl);
                    if is_selected {
                        let border = t.CreateSolidColorBrush(
                            &D2D1_COLOR_F {
                                r: 1.0,
                                g: 1.0,
                                b: 1.0,
                                a: 0.5,
                            },
                            None,
                        )?;
                        t.DrawRoundedRectangle(&cell_rr, &border, 1.0, None);
                    }
                }
                let icon_left = cell.left + (metrics.cell_w - icon_size) / 2.0;
                let icon_rect = D2D_RECT_F {
                    left: icon_left,
                    top: cell.top,
                    right: icon_left + icon_size,
                    bottom: cell.top + icon_size,
                };
                if let Some(bmp) = self.icon_bitmap(item) {
                    t.DrawBitmap(
                        &bmp,
                        Some(&icon_rect),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        None,
                    );
                }
                let label = middle_truncate(&display_name(item), max_chars);
                let label_utf16: Vec<u16> = label.encode_utf16().collect();
                t.DrawText(
                    &label_utf16,
                    &self.label_format,
                    &D2D_RECT_F {
                        left: cell.left - 2.0,
                        top: icon_rect.bottom + 2.0,
                        right: cell.right + 2.0,
                        bottom: cell.bottom,
                    },
                    &text_brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }

            // M11: empty fences teach the interaction.
            if items.is_empty() {
                let hint_brush = t.CreateSolidColorBrush(
                    &D2D1_COLOR_F {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 0.35,
                    },
                    None,
                )?;
                let hint: Vec<u16> = "Drag items here".encode_utf16().collect();
                t.DrawText(
                    &hint,
                    &self.hint_format,
                    &D2D_RECT_F {
                        left: 0.0,
                        top: TITLEBAR_HEIGHT,
                        right: size.width,
                        bottom: size.height,
                    },
                    &hint_brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }

            // M13: rubber-band selection rectangle.
            if let Some(band) = p.band {
                let fill = t.CreateSolidColorBrush(
                    &D2D1_COLOR_F {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 0.08,
                    },
                    None,
                )?;
                t.FillRectangle(&band, &fill);
                let border = t.CreateSolidColorBrush(
                    &D2D1_COLOR_F {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 0.4,
                    },
                    None,
                )?;
                t.DrawRectangle(&band, &border, 1.0, None);
            }

            // M11: same-fence reorder caret at the insertion position.
            if let DropIndicator::Caret(i) = p.drop {
                let (x, ctop, cbottom) = if items.is_empty() {
                    let c = metrics.cell_rect(0);
                    (c.left, c.top - scroll, c.bottom - scroll)
                } else if i >= items.len() {
                    let c = metrics.cell_rect(items.len() - 1);
                    (c.right, c.top - scroll, c.bottom - scroll)
                } else {
                    let c = metrics.cell_rect(i);
                    (c.left, c.top - scroll, c.bottom - scroll)
                };
                let caret = t.CreateSolidColorBrush(
                    &D2D1_COLOR_F {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 0.8,
                    },
                    None,
                )?;
                t.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F {
                            left: x - 2.5,
                            top: ctop + 4.0,
                            right: x + 0.5,
                            bottom: cbottom - 10.0,
                        },
                        radiusX: 1.5,
                        radiusY: 1.5,
                    },
                    &caret,
                );
            }

            // Overflow indicator: thin scrollbar thumb along the right edge,
            // shown only while actively scrolling (v1.2).
            if max_scroll > 0.0 && p.show_scrollbar {
                let track_top = TITLEBAR_HEIGHT + 3.0;
                let track_h = (size.height - track_top - 3.0).max(0.0);
                let thumb_h = ((size.height / content_h) * track_h).max(24.0).min(track_h);
                let thumb_top = track_top + (scroll / max_scroll) * (track_h - thumb_h);
                let bar = t.CreateSolidColorBrush(
                    &D2D1_COLOR_F {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 0.25,
                    },
                    None,
                )?;
                t.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F {
                            left: size.width - 6.0,
                            top: thumb_top,
                            right: size.width - 2.0,
                            bottom: thumb_top + thumb_h,
                        },
                        radiusX: 2.0,
                        radiusY: 2.0,
                    },
                    &bar,
                );
            }
            t.PopAxisAlignedClip();

            // M11: cross-fence drop target ring, over the whole fence.
            if matches!(p.drop, DropIndicator::Ring) {
                let ring = t.CreateSolidColorBrush(
                    &D2D1_COLOR_F {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 0.65,
                    },
                    None,
                )?;
                t.DrawRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F {
                            left: 1.5,
                            top: 1.5,
                            right: size.width - 1.5,
                            bottom: size.height - 1.5,
                        },
                        radiusX: radius,
                        radiusY: radius,
                    },
                    &ring,
                    2.5,
                    None,
                );
            }

            t.EndDraw(None, None)?;
            self.present();
            Ok(())
        }
    }
}
