// Desktop folder enumeration and config reconciliation.
// Items live in the real Desktop folders (user + Public); which fence an
// item belongs to is purely a config assignment keyed by file path.

use std::collections::HashSet;
use std::ffi::c_void;
use std::path::PathBuf;

use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    SHGetKnownFolderPath, FOLDERID_Desktop, FOLDERID_PublicDesktop, KF_FLAG_DEFAULT,
};

use crate::config;

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

/// Assigns any enumerated item that no fence owns to "Unsorted", creating
/// that fence if it doesn't exist. Returns true if the config changed.
/// (Paths are compared case-insensitively; disk paths keep their case.)
pub fn reconcile(items: &[PathBuf]) -> bool {
    config::with(|cfg| {
        let assigned: HashSet<String> = cfg
            .fences
            .iter()
            .flat_map(|f| f.items.iter().map(|s| s.to_lowercase()))
            .collect();
        let new_items: Vec<String> = items
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|s| !assigned.contains(&s.to_lowercase()))
            .collect();
        if new_items.is_empty() {
            return false;
        }
        let unsorted = config::ensure_unsorted(cfg);
        cfg.fences[unsorted].items.extend(new_items);
        true
    })
}
