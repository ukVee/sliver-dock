//! Desktop-entry scanning + apps.toml merge for the apps section.
//!
//! Two inputs:
//!   * `.desktop` files under `/usr/share/applications` and
//!     `~/.local/share/applications` (user dir overrides system).
//!   * `~/.config/sliver-dock/apps.toml` — `pinned` (ordered), `hidden`
//!     (filter-out), `overrides.<id>.{icon,display_name}`.
//!
//! `build_visible` produces the final ordered list to render: pinned ids
//! first (in declared order), then the remaining non-hidden + non-NoDisplay
//! entries sorted by display name. Overrides are applied last so they win
//! over the parsed Name / Icon.
//!
//! No icon resolution here — `icon` is the raw string from the entry
//! (icon-theme name or absolute path). Rendering / theme lookup is Phase 5c.
//! No `Exec` parsing of field codes — Phase 5d strips `%f / %u / ...` at
//! launch time.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;

use gtk4::cairo;
use gtk4::gdk;
use gtk4::gdk_pixbuf::Pixbuf;
use gtk4::prelude::*;
use gtk4::{IconLookupFlags, IconTheme, TextDirection};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    pub exec: Option<String>,
    pub no_display: bool,
    pub hidden: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AppsConfig {
    pub pinned: Vec<String>,
    pub hidden: Vec<String>,
    pub overrides: BTreeMap<String, AppOverride>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct AppOverride {
    pub icon: Option<String>,
    pub display_name: Option<String>,
}

/// Walk the two standard application dirs, parse every `.desktop` file,
/// and return a map keyed by app-id (filename stem). User dir overrides
/// system dir for matching ids — we scan system first and let `insert`
/// replace.
pub fn scan() -> BTreeMap<String, AppEntry> {
    let mut out = BTreeMap::new();
    for dir in scan_dirs() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("desktop") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            let body = match fs::read_to_string(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if let Some(app) = parse_entry(stem, &body) {
                out.insert(app.id.clone(), app);
            }
        }
    }
    out
}

fn scan_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/usr/share/applications")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    dirs
}

/// Hand-rolled `[Desktop Entry]` parser. Honors only the main section
/// (skips `[Desktop Action *]` blocks, which we don't surface). Locale
/// suffixes (`Name[de]`, `Comment[de_DE]`) are ignored — we use the
/// unsuffixed key.
///
/// Returns `None` for non-Application types (Link, Directory).
fn parse_entry(id: &str, body: &str) -> Option<AppEntry> {
    let mut name: Option<String> = None;
    let mut icon: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut no_display = false;
    let mut hidden = false;
    let mut entry_type: Option<String> = None;
    let mut in_section = false;

    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_section = section == "Desktop Entry";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        let value = value.trim();
        if key.contains('[') {
            continue;
        }
        match key {
            "Name" => name = Some(value.to_owned()),
            "Icon" => icon = Some(value.to_owned()),
            "Exec" => exec = Some(value.to_owned()),
            "Type" => entry_type = Some(value.to_owned()),
            "NoDisplay" => no_display = parse_bool(value),
            "Hidden" => hidden = parse_bool(value),
            _ => {}
        }
    }

    if let Some(t) = &entry_type {
        if t != "Application" {
            return None;
        }
    }

    Some(AppEntry {
        id: id.to_owned(),
        name: name.unwrap_or_else(|| id.to_owned()),
        icon,
        exec,
        no_display,
        hidden,
    })
}

fn parse_bool(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "true" | "1")
}

pub fn load_config() -> AppsConfig {
    let Some(path) = config_path() else { return AppsConfig::default() };
    let body = match fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return AppsConfig::default(),
        Err(e) => {
            eprintln!("sliver-dock: cannot read {}: {e}", path.display());
            return AppsConfig::default();
        }
    };
    match toml::from_str(&body) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "sliver-dock: apps.toml parse error in {}: {e}",
                path.display()
            );
            AppsConfig::default()
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("sliver-dock").join("apps.toml"))
}

/// Strip freedesktop field codes (`%f %F %u %U %d %D %n %N %i %c %k %v %m`)
/// from an Exec line. `%%` collapses to a literal `%`. The result is fed to
/// `glib::spawn_command_line_async` which handles shell-quoting itself, so
/// we don't tokenize here.
pub fn strip_exec_field_codes(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some(_) => {}
            None => out.push('%'),
        }
    }
    out
}

/// Resolve an icon spec (raw `Icon=` value or override) to a filesystem
/// path. Three input forms:
///   * absolute path (`/usr/share/...`) — returned as-is.
///   * `~/...` — `$HOME` expansion.
///   * bare name (`firefox`) — looked up via the system icon theme at
///     `size`, returning whatever the theme picks (PNG variant or SVG).
/// `None` if the theme has no match and the spec isn't a real path.
pub fn resolve_icon_path(spec: &str, size: i32) -> Option<PathBuf> {
    if spec.starts_with('/') {
        return Some(PathBuf::from(spec));
    }
    if let Some(rest) = spec.strip_prefix("~/") {
        let home = std::env::var_os("HOME")?;
        return Some(PathBuf::from(home).join(rest));
    }
    let display = gdk::Display::default()?;
    let theme = IconTheme::for_display(&display);
    let paintable = theme.lookup_icon(
        spec,
        &[],
        size,
        1,
        TextDirection::Ltr,
        IconLookupFlags::empty(),
    );
    paintable.file()?.path()
}

/// Load an icon spec into a square cairo `ImageSurface` sized exactly
/// `size × size` (aspect-preserved scaling via gdk-pixbuf). Returns `None`
/// for any failure — missing file, unsupported format, theme miss.
pub fn load_icon_surface(spec: &str, size: i32) -> Option<cairo::ImageSurface> {
    let path = resolve_icon_path(spec, size)?;
    let pixbuf = Pixbuf::from_file_at_scale(&path, size, size, true).ok()?;
    pixbuf_to_cairo(&pixbuf)
}

/// Pixbuf is RGB(A) unpremultiplied; cairo's `ARgb32` is native-endian
/// premultiplied (BGRA on little-endian). Manual byte loop because gtk4
/// dropped the `cairo_set_source_pixbuf` helper that gtk3 had.
fn pixbuf_to_cairo(pixbuf: &Pixbuf) -> Option<cairo::ImageSurface> {
    let w = pixbuf.width();
    let h = pixbuf.height();
    if w <= 0 || h <= 0 {
        return None;
    }
    let n_chan = pixbuf.n_channels();
    let row_stride = pixbuf.rowstride();
    let src = pixbuf.read_pixel_bytes();
    let dst_stride = w * 4;
    let mut buf = vec![0u8; (dst_stride * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let s = (y * row_stride + x * n_chan) as usize;
            let d = (y * dst_stride + x * 4) as usize;
            let r = src[s];
            let g = src[s + 1];
            let b = src[s + 2];
            let a = if n_chan == 4 { src[s + 3] } else { 255 };
            let pr = (r as u16 * a as u16 / 255) as u8;
            let pg = (g as u16 * a as u16 / 255) as u8;
            let pb = (b as u16 * a as u16 / 255) as u8;
            buf[d] = pb;
            buf[d + 1] = pg;
            buf[d + 2] = pr;
            buf[d + 3] = a;
        }
    }
    cairo::ImageSurface::create_for_data(buf, cairo::Format::ARgb32, w, h, dst_stride).ok()
}

/// Apply pinned / hidden / overrides to the scan result. Returned order
/// is: pinned ids first (in declared order), then the rest sorted by
/// display name. Pinning a `Hidden`/`NoDisplay` entry surfaces it (the
/// user explicitly asked); hidden by `apps.toml` always wins.
pub fn build_visible(entries: &BTreeMap<String, AppEntry>, cfg: &AppsConfig) -> Vec<AppEntry> {
    let hidden: HashSet<&str> = cfg.hidden.iter().map(String::as_str).collect();
    let pinned_set: HashSet<&str> = cfg.pinned.iter().map(String::as_str).collect();

    let apply_override = |mut e: AppEntry| -> AppEntry {
        if let Some(o) = cfg.overrides.get(&e.id) {
            if let Some(icon) = &o.icon {
                e.icon = Some(icon.clone());
            }
            if let Some(name) = &o.display_name {
                e.name = name.clone();
            }
        }
        e
    };

    let mut out: Vec<AppEntry> = Vec::new();
    for id in &cfg.pinned {
        if hidden.contains(id.as_str()) {
            continue;
        }
        if let Some(e) = entries.get(id) {
            out.push(apply_override(e.clone()));
        }
    }
    let mut rest: Vec<AppEntry> = entries
        .values()
        .filter(|e| !pinned_set.contains(e.id.as_str()))
        .filter(|e| !hidden.contains(e.id.as_str()))
        .filter(|e| !e.no_display && !e.hidden)
        .cloned()
        .map(apply_override)
        .collect();
    rest.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out.extend(rest);
    out
}
