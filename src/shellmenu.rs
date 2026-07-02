// M13: the real Explorer context menu for desktop items, via IContextMenu.
// Right-clicking an icon shows Open / Open with / Send to / Delete /
// Properties etc. Deleting a file flows back through the M7 watcher, so the
// fence updates itself.

use std::cell::RefCell;
use std::ffi::c_void;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IContextMenu, IContextMenu2, IShellFolder, SHBindToParent, SHParseDisplayName,
    CMF_NORMAL, CMINVOKECOMMANDINFO,
};
use windows::Win32::UI::WindowsAndMessaging::*;

thread_local! {
    // The IContextMenu2 whose popup is currently up: shell submenus
    // (Open with, Send to) populate through HandleMenuMsg forwarding.
    static ACTIVE_MENU2: RefCell<Option<IContextMenu2>> = const { RefCell::new(None) };
}

/// Forward menu bookkeeping messages to the active shell menu, if any.
/// Returns true when the message was consumed.
pub unsafe fn handle_menu_msg(msg: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
    if !matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM) {
        return false;
    }
    ACTIVE_MENU2.with(|m| {
        if let Some(icm2) = &*m.borrow() {
            icm2.HandleMenuMsg(msg, wparam, lparam).is_ok()
        } else {
            false
        }
    })
}

pub unsafe fn show(owner: HWND, path: &str, pt: POINT) {
    let mut wide: Vec<u16> = path.encode_utf16().collect();
    wide.push(0);
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    if SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None).is_err() {
        return;
    }

    let result = (|| -> Result<()> {
        let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
        let folder: IShellFolder = SHBindToParent(pidl, Some(&mut child))?;
        let icm: IContextMenu = folder.GetUIObjectOf(owner, &[child as *const _], None)?;

        let menu = CreatePopupMenu()?;
        icm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL)?;

        ACTIVE_MENU2.with(|m| *m.borrow_mut() = icm.cast::<IContextMenu2>().ok());
        let _ = SetForegroundWindow(owner);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD,
            pt.x,
            pt.y,
            0,
            owner,
            None,
        )
        .0;
        ACTIVE_MENU2.with(|m| *m.borrow_mut() = None);
        let _ = DestroyMenu(menu);

        if cmd > 0 {
            // idCmdFirst was 1, so the verb offset is cmd - 1 (as a
            // MAKEINTRESOURCEA-style pseudo pointer).
            let info = CMINVOKECOMMANDINFO {
                cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                hwnd: owner,
                lpVerb: PCSTR((cmd - 1) as usize as *const u8),
                nShow: SW_SHOWNORMAL.0,
                ..Default::default()
            };
            let _ = icm.InvokeCommand(&info);
        }
        Ok(())
    })();
    let _ = result;

    CoTaskMemFree(Some(pidl as *const c_void));
}
