// Launching items: ShellExecuteW handles .lnk, .exe, .url, folders, documents.

use windows::core::*;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub fn launch(path: &str) {
    let mut wide: Vec<u16> = path.encode_utf16().collect();
    wide.push(0);
    let inst = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Per ShellExecute docs, values <= 32 are error codes.
    if (inst.0 as isize) <= 32 {
        println!("launch failed ({}) for {path}", inst.0 as isize);
    }
}

/// Tray "Settings…" (v1): open config.json in Notepad.
pub fn open_config_in_notepad() {
    let path = crate::config::path();
    let mut wide: Vec<u16> = path.to_string_lossy().encode_utf16().collect();
    wide.push(0);
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("notepad.exe"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
}
