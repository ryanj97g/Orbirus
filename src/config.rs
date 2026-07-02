// Config persistence: %APPDATA%\orbirus\config.json.
// Atomic write (temp file then rename); saves debounced ~500ms via a timer
// on the hidden main window. The in-memory Config is the source of truth,
// owned by the UI thread.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::SetTimer;

pub const SAVE_TIMER_ID: usize = 1;

#[derive(Serialize, Deserialize, Clone)]
pub struct FenceConfig {
    pub id: String,
    pub title: String,
    // Physical pixels, virtual-desktop (screen) coordinates. When a fence is
    // rolled up, `h` keeps the restored height; `rolled_up` carries the state.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub monitor: u32,
    pub rolled_up: bool,
    pub color: String,
    pub opacity: f32,
    pub corner_radius: f32,
    pub items: Vec<String>,
    // v2 (M8): auto-organize rules. Default keeps v1 configs loading.
    #[serde(default)]
    pub rules: Vec<crate::rules::Rule>,
}

#[derive(Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    pub icon_size: u32,
    pub fences: Vec<FenceConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: 2,
            icon_size: 48,
            fences: vec![FenceConfig {
                id: "fence-1".to_string(),
                title: "Unsorted".to_string(),
                x: 100,
                y: 80,
                w: 420,
                h: 300,
                monitor: 0,
                rolled_up: false,
                color: "#1E1E2E".to_string(),
                opacity: 0.78,
                corner_radius: 12.0,
                items: Vec::new(),
                rules: Vec::new(),
            }],
        }
    }
}

/// Next unused "fence-N" id. Takes &Config so it can be used inside a
/// `with` closure without re-entrant borrowing.
pub fn next_id_for(cfg: &Config) -> String {
    let n = cfg
        .fences
        .iter()
        .filter_map(|f| f.id.strip_prefix("fence-")?.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    format!("fence-{n}")
}

/// Index of the "Unsorted" fence, creating it with default geometry if it
/// doesn't exist.
pub fn ensure_unsorted(cfg: &mut Config) -> usize {
    if let Some(i) = cfg.fences.iter().position(|f| f.title == "Unsorted") {
        return i;
    }
    let id = next_id_for(cfg);
    cfg.fences.push(FenceConfig {
        id,
        title: "Unsorted".to_string(),
        x: 100,
        y: 80,
        w: 420,
        h: 300,
        monitor: 0,
        rolled_up: false,
        color: "#1E1E2E".to_string(),
        opacity: 0.78,
        corner_radius: 12.0,
        items: Vec::new(),
        rules: Vec::new(),
    });
    cfg.fences.len() - 1
}

pub enum LoadResult {
    Loaded(Config),
    Missing,
    Corrupt,
}

thread_local! {
    static CURRENT: RefCell<Config> = RefCell::new(Config::default());
    static SAVE_HWND: Cell<isize> = const { Cell::new(0) };
}

pub fn path() -> PathBuf {
    dirs::config_dir()
        .expect("no config directory")
        .join("orbirus")
        .join("config.json")
}

pub fn load() -> LoadResult {
    let text = match fs::read_to_string(path()) {
        Ok(t) => t,
        Err(_) => return LoadResult::Missing,
    };
    // Tolerate a UTF-8 BOM: external editors (invited via Settings…) may
    // prepend one, and serde_json rejects it.
    let text = text.trim_start_matches('\u{feff}');
    match serde_json::from_str::<Config>(text) {
        Ok(mut cfg) => {
            // v1 -> v2 migration (M8): serde's default already gave every
            // fence an empty rules array; just stamp the version.
            cfg.version = 2;
            // Items that vanished from disk are silently dropped (§5).
            for f in &mut cfg.fences {
                f.items.retain(|p| Path::new(p).exists());
            }
            LoadResult::Loaded(cfg)
        }
        Err(e) => {
            // M10: never silently overwrite a layout the user might want
            // back — park the unreadable file next to the fresh one.
            println!("config.json is unreadable ({e}); starting with defaults");
            let bad = path().with_extension("json.bad");
            let _ = fs::remove_file(&bad);
            let _ = fs::rename(path(), &bad);
            LoadResult::Corrupt
        }
    }
}

/// Installs `cfg` as the live config and remembers which window owns the
/// debounce timer.
pub fn init(cfg: Config, save_hwnd: HWND) {
    CURRENT.with(|c| *c.borrow_mut() = cfg);
    SAVE_HWND.with(|h| h.set(save_hwnd.0 as isize));
}

/// M12: swaps in a different config (layout restore) without touching the
/// save-timer owner.
pub fn replace(cfg: Config) {
    CURRENT.with(|c| *c.borrow_mut() = cfg);
}

/// M12: directory holding layout snapshots.
pub fn snapshots_dir() -> PathBuf {
    path().parent().unwrap().join("snapshots")
}

pub fn with<R>(f: impl FnOnce(&mut Config) -> R) -> R {
    CURRENT.with(|c| f(&mut c.borrow_mut()))
}

/// Debounced save: (re)arms a 500ms timer on the main window; WM_TIMER calls
/// `save_now`. Re-arming an existing timer id just resets the countdown.
pub fn schedule_save() {
    SAVE_HWND.with(|h| {
        let raw = h.get();
        if raw != 0 {
            unsafe { SetTimer(HWND(raw as *mut _), SAVE_TIMER_ID, 500, None) };
        }
    });
}

pub fn save_now() {
    CURRENT.with(|c| {
        let cfg = c.borrow();
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(&*cfg) {
            Ok(json) => {
                let tmp = p.with_extension("json.tmp");
                match fs::write(&tmp, json).and_then(|_| fs::rename(&tmp, &p)) {
                    Ok(()) => {}
                    Err(e) => println!("config save failed: {e}"),
                }
            }
            Err(e) => println!("config serialize failed: {e}"),
        }
    });
}
