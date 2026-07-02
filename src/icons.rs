// Icon extraction and caching.
// IShellItemImageFactory::GetImage(SIIGBF_ICONONLY) gives an HBITMAP; we pull
// pixels out via GetDIBits (top-down 32bpp) and premultiply alpha, caching
// CPU-side BGRA per path. Render targets create their own ID2D1Bitmaps from
// these — extraction never happens during paint (only at load/change time).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;

use windows::core::*;
use windows::Win32::Foundation::{FALSE, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, GetDC, GetDIBits, ReleaseDC, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::System::Com::IBindCtx;
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HCURSOR, ICONINFO};

/// Premultiplied BGRA pixels, `size` x `size`.
pub struct IconPixels {
    pub size: u32,
    pub bgra: Vec<u8>,
}

thread_local! {
    static CACHE: RefCell<HashMap<String, Option<IconPixels>>> = RefCell::new(HashMap::new());
    static ICON_PX: Cell<u32> = const { Cell::new(48) };
}

/// Extracts icons for all `paths` at `px` physical pixels (call at startup /
/// on file changes, never from paint).
pub fn preload(paths: &[String], px: u32) {
    ICON_PX.with(|c| c.set(px.max(16)));
    for p in paths {
        ensure_cached(p);
    }
}

fn ensure_cached(path: &str) {
    let missing = CACHE.with(|c| !c.borrow().contains_key(path));
    if missing {
        let px = ICON_PX.with(|c| c.get());
        let pixels = unsafe { extract(path, px) };
        CACHE.with(|c| c.borrow_mut().insert(path.to_string(), pixels));
    }
}

pub fn with_pixels<R>(path: &str, f: impl FnOnce(&IconPixels) -> R) -> Option<R> {
    ensure_cached(path);
    CACHE.with(|c| c.borrow().get(path).and_then(|o| o.as_ref()).map(f))
}

/// Builds a semi-transparent "ghost" cursor from an item's cached icon, used
/// while dragging it between fences. Caller destroys it with DestroyCursor.
pub fn drag_cursor(path: &str) -> Option<HCURSOR> {
    with_pixels(path, |px| unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: px.size as i32,
                biHeight: -(px.size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let hbm_color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        // ~60% opacity ghost: pixels are premultiplied, so scale every channel.
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, px.bgra.len());
        for (d, s) in dst.iter_mut().zip(&px.bgra) {
            *d = (*s as u32 * 3 / 5) as u8;
        }
        let hbm_mask = CreateBitmap(px.size as i32, px.size as i32, 1, 1, None);
        let info = ICONINFO {
            fIcon: FALSE, // a cursor, not an icon
            xHotspot: px.size / 2,
            yHotspot: px.size / 2,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };
        let cursor = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(hbm_color);
        let _ = DeleteObject(hbm_mask);
        cursor.map(|h| HCURSOR(h.0))
    })
    .flatten()
}

unsafe fn extract(path: &str, px: u32) -> Option<IconPixels> {
    let mut wide: Vec<u16> = path.encode_utf16().collect();
    wide.push(0);
    let factory: IShellItemImageFactory =
        SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>).ok()?;
    let hbm = factory
        .GetImage(
            SIZE {
                cx: px as i32,
                cy: px as i32,
            },
            SIIGBF_ICONONLY,
        )
        .ok()?;

    let hdc = GetDC(None);
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: px as i32,
            biHeight: -(px as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bgra = vec![0u8; (px * px * 4) as usize];
    let got = GetDIBits(
        hdc,
        hbm,
        0,
        px,
        Some(bgra.as_mut_ptr() as *mut c_void),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    ReleaseDC(None, hdc);
    let _ = DeleteObject(hbm);
    if got == 0 {
        return None;
    }

    // Some sources return no alpha channel (all zero): treat as opaque.
    // Otherwise premultiply for D2D1_ALPHA_MODE_PREMULTIPLIED.
    if bgra.chunks_exact(4).all(|p| p[3] == 0) {
        for p in bgra.chunks_exact_mut(4) {
            p[3] = 255;
        }
    } else {
        for p in bgra.chunks_exact_mut(4) {
            let a = p[3] as u32;
            p[0] = ((p[0] as u32 * a) / 255) as u8;
            p[1] = ((p[1] as u32 * a) / 255) as u8;
            p[2] = ((p[2] as u32 * a) / 255) as u8;
        }
    }
    Some(IconPixels { size: px, bgra })
}
