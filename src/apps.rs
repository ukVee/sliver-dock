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
    /// Raw `Categories=` list in declared order, empties dropped. Resolution
    /// to a single planet key lives in `categories.rs`.
    pub categories: Vec<String>,
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
    let mut categories: Vec<String> = Vec::new();
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
            // Semicolon-separated and conventionally trailing-terminated
            // (`Network;WebBrowser;`), so the split yields a final empty
            // field we drop along with any stray blanks.
            "Categories" => {
                categories = value
                    .split(';')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
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
        categories,
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

    let apply_override = |e: AppEntry| -> AppEntry { apply_app_override(e, cfg) };

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

fn apply_app_override(mut e: AppEntry, cfg: &AppsConfig) -> AppEntry {
    if let Some(o) = cfg.overrides.get(&e.id) {
        if let Some(icon) = &o.icon {
            e.icon = Some(icon.clone());
        }
        if let Some(name) = &o.display_name {
            e.name = name.clone();
        }
    }
    e
}

/// The full launchable roster, for the astro view — every scanned app the
/// system considers user-launchable, sorted by display name.
///
/// Deliberately NOT `build_visible`: `apps.toml`'s `pinned`/`hidden` lists
/// curate the *dock row* (this device hides 34 of 65 entries so only the 8
/// pinned icons show), and applying that here would make `foot`, `flycast`,
/// `nvim` and the Waydroid apps unreachable from the launcher that exists to
/// reach them. `overrides` still applies — that's presentation, not
/// curation.
///
/// Entries the `.desktop` file itself marks `NoDisplay` / `Hidden` are
/// dropped: those are D-Bus activation stubs and authentication agents
/// (`polkit-gnome-authentication-agent-1`), not apps. The retired grid took
/// the raw registry and listed them; the astro view does not.
pub fn build_launchable(entries: &BTreeMap<String, AppEntry>, cfg: &AppsConfig) -> Vec<AppEntry> {
    let mut out: Vec<AppEntry> = entries
        .values()
        .filter(|e| !e.no_display && !e.hidden)
        .cloned()
        .map(|e| apply_app_override(e, cfg))
        .collect();
    out.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str) -> AppEntry {
        AppEntry {
            id: id.to_string(),
            name: name.to_string(),
            icon: None,
            exec: Some("/usr/bin/true".to_string()),
            no_display: false,
            hidden: false,
            categories: Vec::new(),
        }
    }

    #[test]
    fn parse_categories_drops_trailing_empty() {
        let e = parse_entry("a", "[Desktop Entry]\nName=A\nCategories=GNOME;GTK;Network;\n").unwrap();
        assert_eq!(e.categories, ["GNOME", "GTK", "Network"]);
    }

    #[test]
    fn parse_categories_absent_is_empty() {
        let e = parse_entry("a", "[Desktop Entry]\nName=A\n").unwrap();
        assert!(e.categories.is_empty());
    }

    #[test]
    fn parse_categories_empty_value_is_empty() {
        let e = parse_entry("a", "[Desktop Entry]\nName=A\nCategories=\n").unwrap();
        assert!(e.categories.is_empty());
    }

    #[test]
    fn parse_categories_ignores_locale_suffixed_key() {
        let e = parse_entry("a", "[Desktop Entry]\nCategories[de]=Spiel;\nCategories=Game;\n")
            .unwrap();
        assert_eq!(e.categories, ["Game"]);
    }

    #[test]
    fn strip_empty() {
        assert_eq!(strip_exec_field_codes(""), "");
    }

    #[test]
    fn strip_no_codes() {
        assert_eq!(strip_exec_field_codes("firefox"), "firefox");
    }

    #[test]
    fn strip_percent_f() {
        assert_eq!(strip_exec_field_codes("app %f"), "app ");
    }

    #[test]
    fn strip_percent_u() {
        assert_eq!(strip_exec_field_codes("app %u"), "app ");
    }

    #[test]
    fn strip_percent_caps() {
        assert_eq!(strip_exec_field_codes("app %F"), "app ");
        assert_eq!(strip_exec_field_codes("app %U %F"), "app  ");
    }

    #[test]
    fn strip_double_percent() {
        assert_eq!(strip_exec_field_codes("app %%f"), "app %f");
        assert_eq!(strip_exec_field_codes("%%"), "%");
    }

    #[test]
    fn strip_trailing_percent() {
        assert_eq!(strip_exec_field_codes("app %"), "app %");
    }

    #[test]
    fn build_visible_pinned_first() {
        let mut entries = BTreeMap::new();
        entries.insert("b".to_string(), entry("b", "Beta"));
        entries.insert("a".to_string(), entry("a", "Alpha"));
        let cfg = AppsConfig {
            pinned: vec!["b".to_string()],
            ..Default::default()
        };
        let visible = build_visible(&entries, &cfg);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].id, "b");
        assert_eq!(visible[1].id, "a");
    }

    #[test]
    fn build_visible_hidden_filter() {
        let mut entries = BTreeMap::new();
        entries.insert("a".to_string(), entry("a", "Alpha"));
        entries.insert("b".to_string(), entry("b", "Beta"));
        let cfg = AppsConfig {
            hidden: vec!["b".to_string()],
            ..Default::default()
        };
        let visible = build_visible(&entries, &cfg);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "a");
    }

    #[test]
    fn build_visible_hidden_wins_over_pinned() {
        let mut entries = BTreeMap::new();
        entries.insert("a".to_string(), entry("a", "Alpha"));
        let cfg = AppsConfig {
            pinned: vec!["a".to_string()],
            hidden: vec!["a".to_string()],
            ..Default::default()
        };
        assert!(build_visible(&entries, &cfg).is_empty());
    }

    #[test]
    fn build_visible_no_display_filtered() {
        let mut entries = BTreeMap::new();
        let mut hidden_e = entry("hidden", "Hidden App");
        hidden_e.no_display = true;
        entries.insert("hidden".to_string(), hidden_e);
        entries.insert("shown".to_string(), entry("shown", "Shown App"));
        let visible = build_visible(&entries, &AppsConfig::default());
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "shown");
    }

    #[test]
    fn build_visible_override_name() {
        let mut entries = BTreeMap::new();
        entries.insert("a".to_string(), entry("a", "Original"));
        let mut overrides = BTreeMap::new();
        overrides.insert("a".to_string(), AppOverride {
            display_name: Some("Overridden".to_string()),
            icon: None,
        });
        let cfg = AppsConfig {
            overrides,
            ..Default::default()
        };
        let visible = build_visible(&entries, &cfg);
        assert_eq!(visible[0].name, "Overridden");
    }

    #[test]
    fn build_visible_pinned_surfaces_nodisplay() {
        let mut entries = BTreeMap::new();
        let mut hidden_e = entry("a", "Alpha");
        hidden_e.no_display = true;
        entries.insert("a".to_string(), hidden_e);
        let cfg = AppsConfig {
            pinned: vec!["a".to_string()],
            ..Default::default()
        };
        let visible = build_visible(&entries, &cfg);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "a");
    }

    #[test]
    fn build_launchable_ignores_dock_row_curation() {
        let mut entries = BTreeMap::new();
        entries.insert("foot".to_string(), entry("foot", "foot"));
        entries.insert("firefox".to_string(), entry("firefox", "Firefox"));
        let cfg = AppsConfig {
            pinned: vec!["firefox".to_string()],
            hidden: vec!["foot".to_string()],
            ..Default::default()
        };
        // build_visible honours the dock-row curation...
        let visible = build_visible(&entries, &cfg);
        assert_eq!(visible.len(), 1);
        // ...the launcher roster does not.
        let launchable = build_launchable(&entries, &cfg);
        let ids: Vec<&str> = launchable.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["firefox", "foot"]);
    }

    #[test]
    fn build_launchable_drops_no_display_and_hidden_entries() {
        let mut entries = BTreeMap::new();
        let mut agent = entry("polkit-agent", "Polkit Agent");
        agent.no_display = true;
        entries.insert("polkit-agent".to_string(), agent);
        let mut stub = entry("stub", "Stub");
        stub.hidden = true;
        entries.insert("stub".to_string(), stub);
        entries.insert("real".to_string(), entry("real", "Real App"));
        let launchable = build_launchable(&entries, &AppsConfig::default());
        assert_eq!(launchable.len(), 1);
        assert_eq!(launchable[0].id, "real");
    }

    #[test]
    fn build_launchable_applies_overrides() {
        let mut entries = BTreeMap::new();
        entries.insert("a".to_string(), entry("a", "Original"));
        let mut overrides = BTreeMap::new();
        overrides.insert("a".to_string(), AppOverride {
            display_name: Some("Renamed".to_string()),
            icon: Some("custom-icon".to_string()),
        });
        let cfg = AppsConfig {
            overrides,
            ..Default::default()
        };
        let launchable = build_launchable(&entries, &cfg);
        assert_eq!(launchable[0].name, "Renamed");
        assert_eq!(launchable[0].icon.as_deref(), Some("custom-icon"));
    }

    #[test]
    fn build_launchable_empty_scan() {
        assert!(build_launchable(&BTreeMap::new(), &AppsConfig::default()).is_empty());
    }

    #[test]
    fn build_visible_pinned_unknown_id_ignored() {
        let mut entries = BTreeMap::new();
        entries.insert("a".to_string(), entry("a", "Alpha"));
        let cfg = AppsConfig {
            pinned: vec!["nonexistent".to_string()],
            ..Default::default()
        };
        let visible = build_visible(&entries, &cfg);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "a");
    }
}
