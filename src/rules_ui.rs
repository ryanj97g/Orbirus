// Auto-organize dialog (M8, addendum §11).
// Lives in its own file per the addendum's guidance — fence.rs already
// exceeds ~800 lines. Plain Win32 controls forming the fill-in-the-blank
// sentence:  Move files that [ combo ] into this fence.
// Options 6/7 reveal a text field. Existing rules are listed in a LISTBOX
// with a Remove button acting on the selection (the addendum's sanctioned
// control set; per-row owner-drawn buttons were the alternative).

use std::cell::Cell;
use std::ffi::c_void;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_BTNFACE,
    DEFAULT_CHARSET, HBRUSH, HFONT, OUT_DEFAULT_PRECIS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config;
use crate::rules::{Rule, RuleKind};

const AUTOORG_CLASS: PCWSTR = w!("OrbirusAutoOrg");

const ID_ADD: usize = 1; // default button — Enter adds
const ID_CANCEL: usize = 2;
const ID_REMOVE: usize = 3;
const ID_COMBO: usize = 10;
const ID_EDIT: usize = 11;
const ID_LIST: usize = 12;

// Classic control messages (windows-rs gates these behind Win32_UI_Controls).
const CB_ADDSTRING: u32 = 0x0143;
const CB_GETCURSEL: u32 = 0x0147;
const CB_SETCURSEL: u32 = 0x014E;
const CBN_SELCHANGE: u32 = 1;
const LB_ADDSTRING: u32 = 0x0180;
const LB_RESETCONTENT: u32 = 0x0184;
const LB_GETCURSEL: u32 = 0x0188;

// Exact wording per §11.
const OPTION_LABELS: [&str; 7] = [
    "are pictures",
    "are documents",
    "are apps or shortcuts",
    "are folders",
    "are videos or music",
    "have a name containing…",
    "have the file type…",
];
const OPTION_CATEGORY: [&str; 5] = ["pictures", "documents", "apps", "folders", "media"];

thread_local! {
    static DIALOG_HWND: Cell<isize> = const { Cell::new(0) };
}

/// For the main message loop (IsDialogMessageW routing).
pub fn dialog_hwnd() -> HWND {
    DIALOG_HWND.with(|c| HWND(c.get() as *mut _))
}

struct Ctx {
    fence_id: String,
    combo: isize,
    edit: isize,
    list: isize,
    font: isize,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub unsafe fn register_class(hinstance: HINSTANCE) -> Result<()> {
    let wc = WNDCLASSW {
        lpfnWndProc: Some(autoorg_wndproc),
        hInstance: hinstance,
        lpszClassName: AUTOORG_CLASS,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as isize as *mut c_void),
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return Err(Error::from_win32());
    }
    Ok(())
}

pub unsafe fn open(fence_hwnd: HWND, fence_id: &str) {
    let existing = DIALOG_HWND.with(|c| c.get());
    if existing != 0 {
        let _ = SetForegroundWindow(HWND(existing as *mut _));
        return;
    }
    let Ok(hmodule) = GetModuleHandleW(None) else { return };
    let hinstance: HINSTANCE = hmodule.into();

    let dpi = GetDpiForWindow(fence_hwnd) as i32;
    let s = |v: i32| v * dpi / 96;

    let mut frc = RECT::default();
    let _ = GetWindowRect(fence_hwnd, &mut frc);
    let (dw, dh) = (s(430), s(345));
    let dlg = match CreateWindowExW(
        WS_EX_TOOLWINDOW,
        AUTOORG_CLASS,
        w!("Auto-organize"),
        WS_POPUP | WS_CAPTION | WS_SYSMENU,
        (frc.left + frc.right) / 2 - dw / 2,
        (frc.top + frc.bottom) / 2 - dh / 2,
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

    let mk = |class: PCWSTR, text: PCWSTR, style: WINDOW_STYLE, x: i32, y: i32, w: i32, h: i32, id: usize| -> HWND {
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

    let _ = mk(w!("STATIC"), w!("Move files that"), WINDOW_STYLE(0), 10, 16, 96, 20, 0);
    let combo = mk(
        w!("COMBOBOX"),
        PCWSTR::null(),
        WINDOW_STYLE((CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32) | WS_VSCROLL,
        110,
        12,
        170,
        220,
        ID_COMBO,
    );
    let _ = mk(w!("STATIC"), w!("into this fence."), WINDOW_STYLE(0), 286, 16, 110, 20, 0);
    let edit = mk(
        w!("EDIT"),
        PCWSTR::null(),
        WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
        110,
        44,
        170,
        24,
        ID_EDIT,
    );
    let _ = ShowWindow(edit, SW_HIDE); // revealed by options 6/7
    let _ = mk(
        w!("BUTTON"),
        w!("Add rule"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
        10,
        78,
        90,
        26,
        ID_ADD,
    );
    let _ = mk(w!("BUTTON"), w!("Cancel"), WINDOW_STYLE(0), 108, 78, 80, 26, ID_CANCEL);
    let list = mk(
        w!("LISTBOX"),
        PCWSTR::null(),
        WS_BORDER | WS_VSCROLL | WINDOW_STYLE(LBS_NOTIFY as u32),
        10,
        116,
        302,
        180,
        ID_LIST,
    );
    let _ = mk(w!("BUTTON"), w!("Remove"), WINDOW_STYLE(0), 320, 116, 84, 26, ID_REMOVE);

    for label in OPTION_LABELS {
        let w16 = wide(label);
        SendMessageW(combo, CB_ADDSTRING, WPARAM(0), LPARAM(w16.as_ptr() as isize));
    }
    SendMessageW(combo, CB_SETCURSEL, WPARAM(0), LPARAM(0));

    let ctx = Box::new(Ctx {
        fence_id: fence_id.to_string(),
        combo: combo.0 as isize,
        edit: edit.0 as isize,
        list: list.0 as isize,
        font: font.0 as isize,
    });
    refresh_list(&ctx);
    SetWindowLongPtrW(dlg, GWLP_USERDATA, Box::into_raw(ctx) as isize);
    DIALOG_HWND.with(|c| c.set(dlg.0 as isize));

    let _ = ShowWindow(dlg, SW_SHOW);
    let _ = SetForegroundWindow(dlg);
    let _ = SetFocus(combo);
}

fn label_for_rule(rule: &Rule) -> String {
    match rule.kind {
        RuleKind::Category => OPTION_CATEGORY
            .iter()
            .position(|c| *c == rule.value)
            .map(|i| OPTION_LABELS[i].to_string())
            .unwrap_or_else(|| rule.value.clone()),
        RuleKind::NameContains => format!("have a name containing \"{}\"", rule.value),
        RuleKind::Extension => format!("have the file type \"{}\"", rule.value),
    }
}

unsafe fn refresh_list(ctx: &Ctx) {
    let list = HWND(ctx.list as *mut _);
    SendMessageW(list, LB_RESETCONTENT, WPARAM(0), LPARAM(0));
    let labels: Vec<String> = config::with(|cfg| {
        cfg.fences
            .iter()
            .find(|f| f.id == ctx.fence_id)
            .map(|f| f.rules.iter().map(label_for_rule).collect())
            .unwrap_or_default()
    });
    for label in labels {
        let w16 = wide(&label);
        SendMessageW(list, LB_ADDSTRING, WPARAM(0), LPARAM(w16.as_ptr() as isize));
    }
}

unsafe fn rule_from_ui(ctx: &Ctx) -> Option<Rule> {
    let sel = SendMessageW(HWND(ctx.combo as *mut _), CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
    if !(0..7).contains(&sel) {
        return None;
    }
    if sel <= 4 {
        return Some(Rule {
            kind: RuleKind::Category,
            value: OPTION_CATEGORY[sel as usize].to_string(),
        });
    }
    let mut buf = [0u16; 256];
    let len = GetWindowTextW(HWND(ctx.edit as *mut _), &mut buf);
    let text = String::from_utf16_lossy(&buf[..len as usize])
        .trim()
        .to_string();
    if text.is_empty() {
        return None;
    }
    if sel == 5 {
        Some(Rule {
            kind: RuleKind::NameContains,
            value: text,
        })
    } else {
        // Extension: accept with or without the leading dot; store lowercase
        // without it.
        let v = text.trim_start_matches('.').to_lowercase();
        if v.is_empty() {
            return None;
        }
        Some(Rule {
            kind: RuleKind::Extension,
            value: v,
        })
    }
}

unsafe fn add_rule(dlg: HWND) {
    let ctx = GetWindowLongPtrW(dlg, GWLP_USERDATA) as *mut Ctx;
    let Some(ctx) = ctx.as_ref() else { return };
    let Some(rule) = rule_from_ui(ctx) else { return };

    // Identical rule on any fence (including this one) blocks the add and
    // names the holder (§11 conflict handling).
    let holder: Option<String> = config::with(|cfg| {
        cfg.fences
            .iter()
            .find(|f| f.rules.iter().any(|r| r.same_as(&rule)))
            .map(|f| f.title.clone())
    });
    if let Some(title) = holder {
        let text = wide(&format!("The fence \"{title}\" already has this rule."));
        MessageBoxW(dlg, PCWSTR(text.as_ptr()), w!("Orbirus"), MB_OK | MB_ICONINFORMATION);
        return;
    }

    config::with(|cfg| {
        if let Some(f) = cfg.fences.iter_mut().find(|f| f.id == ctx.fence_id) {
            f.rules.push(rule);
        }
    });
    config::schedule_save();
    refresh_list(ctx);
    let _ = SetWindowTextW(HWND(ctx.edit as *mut _), w!(""));
}

unsafe fn remove_selected(dlg: HWND) {
    let ctx = GetWindowLongPtrW(dlg, GWLP_USERDATA) as *mut Ctx;
    let Some(ctx) = ctx.as_ref() else { return };
    let sel = SendMessageW(HWND(ctx.list as *mut _), LB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
    if sel < 0 {
        return;
    }
    config::with(|cfg| {
        if let Some(f) = cfg.fences.iter_mut().find(|f| f.id == ctx.fence_id) {
            if (sel as usize) < f.rules.len() {
                f.rules.remove(sel as usize);
            }
        }
    });
    config::schedule_save();
    refresh_list(ctx);
}

extern "system" fn autoorg_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = wparam.0 & 0xFFFF;
                let code = (wparam.0 >> 16) as u32;
                match id {
                    ID_ADD => add_rule(hwnd),
                    ID_CANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    ID_REMOVE => remove_selected(hwnd),
                    ID_COMBO if code == CBN_SELCHANGE => {
                        let ctx = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ctx;
                        if let Some(ctx) = ctx.as_ref() {
                            let sel = SendMessageW(
                                HWND(ctx.combo as *mut _),
                                CB_GETCURSEL,
                                WPARAM(0),
                                LPARAM(0),
                            )
                            .0;
                            let edit = HWND(ctx.edit as *mut _);
                            if sel >= 5 {
                                let _ = ShowWindow(edit, SW_SHOW);
                                let _ = SetFocus(edit);
                            } else {
                                let _ = ShowWindow(edit, SW_HIDE);
                            }
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
