# Orbirus

**The executable is target/release/orbirus.exe. Debug builds keep a console for logging; release builds run without one.**

A native Windows desktop organizer. Orbirus draws translucent, labeled panels — "fences" — on the desktop and groups your desktop icons inside them. The icons stay real desktop items; Orbirus only controls how they are grouped and shown. Written in Rust with Win32 and Direct2D — no Electron, no bundled runtime.

## How it works

Orbirus reads the items in your Desktop folders (your user Desktop and the Public Desktop) and displays them inside fences. Which item belongs to which fence is stored in Orbirus's own config, keyed by file path — **files are never moved or renamed on disk.** Items not assigned to a fence appear in a default "Unsorted" fence, so nothing is ever hidden. Fences sit above the wallpaper and behind normal windows.

To avoid seeing every icon twice, hide Windows' own desktop icons: right-click the desktop → View → uncheck "Show desktop icons." Orbirus shows this reminder on first run.

## Features

### Fences
- Translucent, labeled, rounded panels pinned to the desktop layer (behind windows, above the wallpaper).
- Per-pixel transparency: the panel background shows the wallpaper through it while icons and text stay opaque.
- Move (drag the title bar), resize (drag any edge), rename, and roll up to just the title bar (double-click the title bar, or click the chevron).
- Fences snap to each other's edges when moving or resizing.
- Rolling a fence open pushes any rolled fences it covers out of the way; hovering a rolled fence peeks it open.

### Icons
- Double-click to launch (shortcuts, executables, folders, documents, URLs).
- Drag icons between fences, or reorder them within a fence.
- Multi-select with Ctrl+click or a rubber-band drag, then drag the whole selection at once.
- Right-click an icon for the real Windows Explorer context menu (Open, Delete, Properties, and so on).
- Hover highlight, and a tooltip showing the full name when a label is truncated.
- "Sort by color" arranges a fence's icons in rainbow order.

### Automatic organization
- Each fence can hold sorting rules. A new file on the desktop is matched against every fence's rules in order; the first match decides where it goes, otherwise it lands in Unsorted.
- Rule types: by category (pictures, documents, apps/shortcuts, folders, videos/music), by name containing text, or by file extension.
- "Sort Unsorted now" applies the rules to everything currently in Unsorted, with a confirmation that shows where items will go, plus an undo.
- On first run, Orbirus creates five starter fences (Apps, Documents, Pictures, Folders, Unsorted) and sorts your existing desktop items into them.
- Manually placed icons are never re-sorted automatically.

### Customization
- Color, opacity, and corner radius (applied to all fences).
- Icon size: Small, Medium, or Large.
- A Settings window from the tray, and per-fence options from a fence's title-bar menu.

### Live and persistent
- Watches the Desktop folders: files added or removed appear or disappear within about a second.
- Layout, colors, and rules save automatically and restore on launch.
- Save and restore named layout snapshots.
- Fences left off-screen (for example after a monitor change) are pulled back on-screen.

### System integration
- Runs from the system tray; there is no main window.
- Optional "Start with Windows."
- Adds "Orbirus Fence" to the desktop's right-click New menu.
- Single instance: a second launch focuses the running one instead of starting another.
- Per-monitor DPI aware and multi-monitor aware.

## Requirements

- Windows 10 or 11.
- To build: Rust (stable, MSVC toolchain) and the Visual Studio C++ Build Tools.

## Build

```
cargo build --release
```

The executable is `target/release/orbirus.exe`. Debug builds keep a console for logging; release builds run without one.

## Configuration

Settings live at `%APPDATA%\orbirus\config.json` — fence positions (physical pixels), colors, opacity, corner radius, rules, and item assignments. The file is written atomically and can be edited by hand while Orbirus is closed. A config that fails to parse is set aside as `config.json.bad` rather than overwritten.

## Technical notes

- Pure native Win32 with Direct2D/DirectWrite rendering. No web runtime and no background services.
- Idle CPU is effectively 0%; memory use is a few tens of MB.

## License

MIT — see [LICENSE](LICENSE).
