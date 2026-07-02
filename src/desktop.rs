// Desktop folder enumeration and config reconciliation.
// Items live in the real Desktop folders (user + Public); which fence an
// item belongs to is purely a config assignment keyed by file path.

use std::collections::HashSet;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    SHGetKnownFolderPath, FOLDERID_Desktop, FOLDERID_PublicDesktop, KF_FLAG_DEFAULT,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::config;

/// Posted to the main window when either Desktop folder changes (§6.4).
pub const WM_DESKTOP_CHANGED: u32 = WM_APP + 1;

pub fn desktop_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for id in [&FOLDERID_Desktop, &FOLDERID_PublicDesktop] {
        unsafe {
            if let Ok(pw) = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) {
                if let Ok(s) = pw.to_string() {
                    dirs.push(PathBuf::from(s));
                }
                CoTaskMemFree(Some(pw.0 as *const c_void));
            }
        }
    }
    dirs
}

/// Every item in both Desktop folders, minus desktop.ini.
pub fn enumerate() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in desktop_dirs() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("desktop.ini")
                {
                    continue;
                }
                out.push(entry.path());
            }
        }
    }
    out
}

/// Diffs the enumerated desktop against the config: items that vanished from
/// disk are dropped from every fence; unassigned items go to "Unsorted"
/// (created if needed). Returns (changed, newly added paths). Paths compare
/// case-insensitively; disk paths keep their case.
pub fn refresh(items: &[PathBuf]) -> (bool, Vec<String>) {
    config::with(|cfg| {
        let on_disk: HashSet<String> = items
            .iter()
            .map(|p| p.to_string_lossy().to_lowercase())
            .collect();
        let mut changed = false;
        for f in &mut cfg.fences {
            let before = f.items.len();
            f.items.retain(|p| on_disk.contains(&p.to_lowercase()));
            changed |= f.items.len() != before;
        }
        let assigned: HashSet<String> = cfg
            .fences
            .iter()
            .flat_map(|f| f.items.iter().map(|s| s.to_lowercase()))
            .collect();
        let added: Vec<String> = items
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|s| !assigned.contains(&s.to_lowercase()))
            .collect();
        if !added.is_empty() {
            let unsorted = config::ensure_unsorted(cfg);
            cfg.fences[unsorted].items.extend(added.iter().cloned());
            changed = true;
        }
        (changed, added)
    })
}

/// Watches both Desktop folders on background threads; each change posts
/// WM_DESKTOP_CHANGED to `notify_hwnd` (the main thread re-enumerates).
pub fn start_watcher(notify_hwnd: HWND) {
    for dir in desktop_dirs() {
        let hwnd_raw = notify_hwnd.0 as isize;
        std::thread::spawn(move || watch_dir(dir, hwnd_raw));
    }
}

fn watch_dir(dir: PathBuf, hwnd_raw: isize) {
    unsafe {
        let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
        wide.push(0);
        let Ok(handle) = CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        ) else {
            println!("watcher: cannot open {}", dir.display());
            return;
        };
        // DWORD-aligned buffer for FILE_NOTIFY_INFORMATION records.
        let mut buf = [0u32; 1024];
        loop {
            let mut returned = 0u32;
            if ReadDirectoryChangesW(
                handle,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 4) as u32,
                false,
                FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME,
                Some(&mut returned),
                None,
                None,
            )
            .is_err()
            {
                println!("watcher: stopped for {}", dir.display());
                return;
            }
            let _ = PostMessageW(
                HWND(hwnd_raw as *mut _),
                WM_DESKTOP_CHANGED,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
    }
}
