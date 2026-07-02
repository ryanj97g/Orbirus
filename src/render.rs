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
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use crate::icons;

pub const TITLEBAR_HEIGHT: f32 = 28.0; // DIPs

/// Icon grid geometry, all in DIPs. Shared by drawing and hit-testing so a
/// double-click always maps to the cell the user sees.
pub struct GridMetrics {
    pub pad: f32,
    pub cell_w: f32,
    pub cell_h: f32,
    pub cols: usize,
    pub top: f32,
}

pub fn grid_metrics(width_dips: f32, icon_size: f32) -> GridMetrics {
    let pad = 10.0;
    let cell_w = icon_size + 28.0;
    let cell_h = icon_size + 22.0;
    let cols = (((width_dips - 2.0 * pad) / cell_w).floor() as usize).max(1);
    GridMetrics {
        pad,
        cell_w,
        cell_h,
        cols,
        top: TITLEBAR_HEIGHT + 8.0,
    }
}

impl GridMetrics {
    pub fn cell_rect(&self, i: usize) -> D2D_RECT_F {
        let row = i / self.cols;
        let col = i % self.cols;
        let left = self.pad + col as f32 * self.cell_w;
        let top = self.top + row as f32 * self.cell_h;
        D2D_RECT_F {
            left,
            top,
            right: left + self.cell_w,
            bottom: top + self.cell_h,
        }
    }

    pub fn index_at(&self, x: f32, y: f32) -> Option<usize> {
        if y < self.top || x < self.pad {
            return None;
        }
        let col = ((x - self.pad) / self.cell_w) as usize;
        if col >= self.cols {
            return None;
        }
        let row = ((y - self.top) / self.cell_h) as usize;
        Some(row * self.cols + col)
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
    target: ID2D1HwndRenderTarget,
    title_format: IDWriteTextFormat,
    label_format: IDWriteTextFormat,
    // ID2D1Bitmaps are per-render-target resources; each renderer creates its
    // own from the shared CPU-side pixel cache in icons.rs. None = failed,
    // don't retry every paint.
    bitmaps: RefCell<HashMap<String, Option<ID2D1Bitmap>>>,
}

impl FenceRenderer {
    pub fn new(hwnd: HWND) -> Result<Self> {
        unsafe {
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc)?;

            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: 0.0,
                dpiY: 0.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U {
                    width: (rc.right - rc.left).max(1) as u32,
                    height: (rc.bottom - rc.top).max(1) as u32,
                },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let target = d2d_factory()?.CreateHwndRenderTarget(&props, &hwnd_props)?;

            let dpi = GetDpiForWindow(hwnd) as f32;
            target.SetDpi(dpi, dpi);

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

            Ok(Self {
                target,
                title_format,
                label_format,
                bitmaps: RefCell::new(HashMap::new()),
            })
        }
    }

    pub fn resize(&self, width_px: u32, height_px: u32) {
        unsafe {
            let _ = self.target.Resize(&D2D_SIZE_U {
                width: width_px.max(1),
                height: height_px.max(1),
            });
        }
    }

    pub fn set_dpi(&self, dpi: f32) {
        unsafe { self.target.SetDpi(dpi, dpi) };
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
    /// the brush alpha, title bar strip, title text) and the icon grid.
    pub fn draw(
        &self,
        title: &str,
        color: D2D1_COLOR_F,
        opacity: f32,
        radius: f32,
        items: &[String],
        icon_size: f32,
    ) -> Result<()> {
        unsafe {
            let t = &self.target;
            t.BeginDraw();
            t.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
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

            // Icon grid: bitmap centered in each cell, label below,
            // middle-truncated to the cell width.
            let metrics = grid_metrics(size.width, icon_size);
            let max_chars = (metrics.cell_w / 6.0) as usize;
            for (i, item) in items.iter().enumerate() {
                let cell = metrics.cell_rect(i);
                if cell.top > size.height {
                    break;
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

            t.EndDraw(None, None)
        }
    }
}
