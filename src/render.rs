// Direct2D / DirectWrite rendering for fence windows.
// One process-wide D2D factory + one DirectWrite factory (UI thread only);
// per-fence, an ID2D1HwndRenderTarget wrapped in FenceRenderer.

use std::cell::OnceCell;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

pub const TITLEBAR_HEIGHT: f32 = 28.0; // DIPs

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

            Ok(Self {
                target,
                title_format,
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

    /// Draws the fence chrome: rounded-rect background (fence opacity baked
    /// into the brush alpha), title bar strip, and title text.
    pub fn draw(&self, title: &str, color: D2D1_COLOR_F, opacity: f32, radius: f32) -> Result<()> {
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

            t.EndDraw(None, None)
        }
    }
}
