//! Category resolution — turns the flat app roster into astro-view planets.
//!
//! Two inputs:
//!   * each app's `.desktop` `Categories=` list (parsed in `apps.rs`).
//!   * `~/.config/sliver-dock/categories.toml` — `override.<app-id> =
//!     "<category>"` plus optional per-planet `[planet.<category>]
//!     display_name`. Shape mirrors `apps.toml` conventions.
//!
//! ```toml
//! [override]
//! # app-ids with dots MUST be quoted — TOML reads a bare `a.b.c` key as
//! # nested tables, which fails to parse as an app-id → category map.
//! "waydroid.org.fdroid.fdroid" = "Android"
//! "com.libretro.RetroArch" = "Game"
//!
//! [planet.Android]
//! display_name = "Droid"
//! ```
//!
//! Resolution order per app: the `categories.toml` override wins outright,
//! else the first *main* category from the `.desktop` list, else `Other`.
//!
//! Front-of-globe ordering is baked in here: apps within a planet sort by
//! launch count (from `launches.toml`) descending, so slice 4 can lay the
//! most-used apps out from the centre without re-deriving anything.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use crate::apps::AppEntry;

/// Planet for apps with no usable category.
pub const OTHER: &str = "Other";

/// The freedesktop menu-spec "Main" categories (spec table 1) — the only
/// values that may name a planet on their own.
///
/// "First category wins" (note 010) has to mean the first *meaningful* one:
/// on this device 11 of 42 system entries lead with something that isn't a
/// real category — a registered environment (`GNOME;Network;`,
/// `Qt;Utility;`), the deprecated `Application` (`Application;Maps;Utility;`),
/// an Additional category (`TextEditor;Development;IDE;`) or a vendor
/// extension (`X-WayDroid-App;Utility;`). Taking those literally would spawn
/// `GNOME` and `Qt` planets while burying Network and Utility, so anything
/// outside this list is skipped and resolution falls through to the next
/// entry. A `categories.toml` override is still free to name any string it
/// likes — the user's word is final.
const MAIN_CATEGORIES: &[&str] = &[
    "AudioVideo",
    "Audio",
    "Video",
    "Development",
    "Education",
    "Game",
    "Graphics",
    "Network",
    "Office",
    "Science",
    "Settings",
    "System",
    "Utility",
];

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CategoriesConfig {
    /// app-id → category name. Wins over the `.desktop` value.
    #[serde(rename = "override", alias = "overrides")]
    pub overrides: BTreeMap<String, String>,
    /// category key → per-planet presentation.
    pub planet: BTreeMap<String, PlanetOverride>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct PlanetOverride {
    /// Label to render instead of the raw category key. Note 010 spells this
    /// `display-name`; `apps.toml` spells its equivalent `display_name`, so
    /// both are accepted and the snake_case form is canonical.
    #[serde(alias = "display-name")]
    pub display_name: Option<String>,
}

/// One planet: a category and the apps that resolved to it.
#[derive(Debug, Clone, PartialEq)]
pub struct Planet {
    /// Category key — the grouping identity, and what `categories.toml`
    /// `[planet.<key>]` addresses.
    pub key: String,
    /// Label to render. Defaults to `key`.
    pub display_name: String,
    /// Most-launched first (see `build_planets`).
    pub apps: Vec<AppEntry>,
}

pub fn load_config() -> CategoriesConfig {
    let Some(path) = config_path() else {
        return CategoriesConfig::default();
    };
    let body = match fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CategoriesConfig::default(),
        Err(e) => {
            eprintln!("sliver-dock: cannot read {}: {e}", path.display());
            return CategoriesConfig::default();
        }
    };
    match toml::from_str(&body) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "sliver-dock: categories.toml parse error in {}: {e}",
                path.display()
            );
            // Most app-ids on a real roster are dotted (`com.libretro.RetroArch`,
            // `waydroid.org.fdroid.fdroid`), and TOML reads a bare dotted key as
            // nested tables rather than one id — so the overwhelmingly likely
            // mistake is a missing pair of quotes. Say so instead of leaving the
            // user with a bare column number.
            eprintln!(
                "sliver-dock: hint: app-ids containing dots must be quoted — \
                 [override]\\n\"com.libretro.RetroArch\" = \"Game\""
            );
            CategoriesConfig::default()
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("sliver-dock").join("categories.toml"))
}

/// Resolve one app to its planet key: override, else first main category,
/// else `Other`. An override that is blank/whitespace is ignored rather than
/// producing a nameless planet.
pub fn resolve(entry: &AppEntry, cfg: &CategoriesConfig) -> String {
    if let Some(over) = cfg.overrides.get(&entry.id) {
        let over = over.trim();
        if !over.is_empty() {
            return over.to_owned();
        }
    }
    entry
        .categories
        .iter()
        .find(|c| MAIN_CATEGORIES.contains(&c.as_str()))
        .cloned()
        .unwrap_or_else(|| OTHER.to_owned())
}

/// Group the visible roster into planets.
///
/// Apps within a planet: launch count descending, then display name, then id
/// — so the ordering is total and the same roster always yields the same sky.
/// Planets: `Other` last (it's the leftovers bin), the rest by display name
/// then key.
pub fn build_planets(
    visible: &[AppEntry],
    cfg: &CategoriesConfig,
    launches: &HashMap<String, u64>,
) -> Vec<Planet> {
    let mut grouped: BTreeMap<String, Vec<AppEntry>> = BTreeMap::new();
    for entry in visible {
        grouped
            .entry(resolve(entry, cfg))
            .or_default()
            .push(entry.clone());
    }

    let mut planets: Vec<Planet> = grouped
        .into_iter()
        .map(|(key, mut apps)| {
            apps.sort_by(|a, b| {
                let (ca, cb) = (
                    launches.get(&a.id).copied().unwrap_or(0),
                    launches.get(&b.id).copied().unwrap_or(0),
                );
                cb.cmp(&ca)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                    .then_with(|| a.id.cmp(&b.id))
            });
            let display_name = cfg
                .planet
                .get(&key)
                .and_then(|p| p.display_name.clone())
                .unwrap_or_else(|| key.clone());
            Planet {
                key,
                display_name,
                apps,
            }
        })
        .collect();

    planets.sort_by(|a, b| {
        let (oa, ob) = (a.key == OTHER, b.key == OTHER);
        oa.cmp(&ob)
            .then_with(|| {
                a.display_name
                    .to_lowercase()
                    .cmp(&b.display_name.to_lowercase())
            })
            .then_with(|| a.key.cmp(&b.key))
    });
    planets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str, categories: &[&str]) -> AppEntry {
        AppEntry {
            id: id.to_string(),
            name: name.to_string(),
            icon: None,
            exec: Some("/usr/bin/true".to_string()),
            no_display: false,
            hidden: false,
            multi_instance: false,
            categories: categories.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn cfg_with_override(id: &str, category: &str) -> CategoriesConfig {
        let mut overrides = BTreeMap::new();
        overrides.insert(id.to_string(), category.to_string());
        CategoriesConfig {
            overrides,
            ..Default::default()
        }
    }

    #[test]
    fn resolve_first_main_category_wins() {
        let e = entry("a", "A", &["Network", "WebBrowser"]);
        assert_eq!(resolve(&e, &CategoriesConfig::default()), "Network");
    }

    #[test]
    fn resolve_skips_registered_environment() {
        // firefox.desktop: Categories=GNOME;GTK;Network;WebBrowser;
        let e = entry("firefox", "Firefox", &["GNOME", "GTK", "Network", "WebBrowser"]);
        assert_eq!(resolve(&e, &CategoriesConfig::default()), "Network");
    }

    #[test]
    fn resolve_skips_deprecated_application() {
        // xgps.desktop: Categories=Application;Maps;Utility;
        let e = entry("xgps", "xgps", &["Application", "Maps", "Utility"]);
        assert_eq!(resolve(&e, &CategoriesConfig::default()), "Utility");
    }

    #[test]
    fn resolve_skips_additional_category() {
        // code-oss.desktop: Categories=TextEditor;Development;IDE;
        let e = entry("code-oss", "Code", &["TextEditor", "Development", "IDE"]);
        assert_eq!(resolve(&e, &CategoriesConfig::default()), "Development");
    }

    #[test]
    fn resolve_skips_vendor_extension() {
        // Waydroid.desktop: Categories=X-WayDroid-App;Utility;
        let e = entry("Waydroid", "Waydroid", &["X-WayDroid-App", "Utility"]);
        assert_eq!(resolve(&e, &CategoriesConfig::default()), "Utility");
    }

    #[test]
    fn resolve_no_main_category_is_other() {
        let e = entry("a", "A", &["X-WayDroid-App"]);
        assert_eq!(resolve(&e, &CategoriesConfig::default()), OTHER);
    }

    #[test]
    fn resolve_no_categories_is_other() {
        let e = entry("a", "A", &[]);
        assert_eq!(resolve(&e, &CategoriesConfig::default()), OTHER);
    }

    #[test]
    fn resolve_override_wins_over_desktop() {
        let e = entry("a", "A", &["Network"]);
        assert_eq!(resolve(&e, &cfg_with_override("a", "Office")), "Office");
    }

    #[test]
    fn resolve_override_may_name_a_non_main_category() {
        let e = entry("a", "A", &["Network"]);
        assert_eq!(resolve(&e, &cfg_with_override("a", "Android")), "Android");
    }

    #[test]
    fn resolve_override_rescues_an_other_app() {
        let e = entry("a", "A", &["X-WayDroid-App"]);
        assert_eq!(resolve(&e, &cfg_with_override("a", "Android")), "Android");
    }

    #[test]
    fn resolve_blank_override_falls_through() {
        let e = entry("a", "A", &["Network"]);
        assert_eq!(resolve(&e, &cfg_with_override("a", "   ")), "Network");
    }

    #[test]
    fn build_planets_groups_by_category() {
        let visible = vec![
            entry("a", "A", &["Network"]),
            entry("b", "B", &["Utility"]),
            entry("c", "C", &["Network"]),
        ];
        let planets = build_planets(&visible, &CategoriesConfig::default(), &HashMap::new());
        assert_eq!(planets.len(), 2);
        assert_eq!(planets[0].key, "Network");
        assert_eq!(planets[0].apps.len(), 2);
        assert_eq!(planets[1].key, "Utility");
        assert_eq!(planets[1].apps.len(), 1);
    }

    #[test]
    fn build_planets_other_sorts_last() {
        let visible = vec![
            entry("z", "Z", &[]),
            entry("a", "A", &["Utility"]),
            entry("b", "B", &["AudioVideo"]),
        ];
        let planets = build_planets(&visible, &CategoriesConfig::default(), &HashMap::new());
        let keys: Vec<&str> = planets.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["AudioVideo", "Utility", OTHER]);
    }

    #[test]
    fn build_planets_apps_ordered_by_launch_count() {
        let visible = vec![
            entry("rare", "Rare", &["Network"]),
            entry("hot", "Hot", &["Network"]),
            entry("mid", "Mid", &["Network"]),
        ];
        let launches = HashMap::from([("hot".to_string(), 90), ("mid".to_string(), 5)]);
        let planets = build_planets(&visible, &CategoriesConfig::default(), &launches);
        let ids: Vec<&str> = planets[0].apps.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["hot", "mid", "rare"]);
    }

    #[test]
    fn build_planets_ties_break_by_name_then_id() {
        let visible = vec![
            entry("z", "Alpha", &["Network"]),
            entry("a", "Alpha", &["Network"]),
            entry("m", "Beta", &["Network"]),
        ];
        let planets = build_planets(&visible, &CategoriesConfig::default(), &HashMap::new());
        let ids: Vec<&str> = planets[0].apps.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["a", "z", "m"]);
    }

    #[test]
    fn build_planets_display_name_override() {
        let visible = vec![entry("a", "A", &["Development"])];
        let mut planet = BTreeMap::new();
        planet.insert(
            "Development".to_string(),
            PlanetOverride {
                display_name: Some("Code".to_string()),
            },
        );
        let cfg = CategoriesConfig {
            planet,
            ..Default::default()
        };
        let planets = build_planets(&visible, &cfg, &HashMap::new());
        assert_eq!(planets[0].key, "Development");
        assert_eq!(planets[0].display_name, "Code");
    }

    #[test]
    fn build_planets_display_name_defaults_to_key() {
        let visible = vec![entry("a", "A", &["Development"])];
        let planets = build_planets(&visible, &CategoriesConfig::default(), &HashMap::new());
        assert_eq!(planets[0].display_name, "Development");
    }

    #[test]
    fn build_planets_empty_roster() {
        assert!(build_planets(&[], &CategoriesConfig::default(), &HashMap::new()).is_empty());
    }

    #[test]
    fn config_parses_note_010_shape() {
        let cfg: CategoriesConfig = toml::from_str(
            r#"
            [override]
            Waydroid = "Android"
            foot = "System"

            [planet.Android]
            display_name = "Droid"

            [planet.Network]
            display-name = "Web"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.overrides.get("Waydroid").unwrap(), "Android");
        assert_eq!(cfg.overrides.get("foot").unwrap(), "System");
        assert_eq!(
            cfg.planet.get("Android").unwrap().display_name.as_deref(),
            Some("Droid")
        );
        // hyphenated alias from note 010's prose
        assert_eq!(
            cfg.planet.get("Network").unwrap().display_name.as_deref(),
            Some("Web")
        );
    }

    #[test]
    fn config_parses_quoted_dotted_app_ids() {
        // The common case on this device: reverse-DNS and waydroid.* ids.
        let cfg: CategoriesConfig = toml::from_str(
            r#"
            [override]
            "waydroid.org.fdroid.fdroid" = "Android"
            "com.libretro.RetroArch" = "Game"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.overrides.get("waydroid.org.fdroid.fdroid").unwrap(), "Android");
        assert_eq!(cfg.overrides.get("com.libretro.RetroArch").unwrap(), "Game");
    }

    #[test]
    fn config_unquoted_dotted_app_id_is_rejected_not_silently_mangled() {
        // A bare dotted key is nested tables, not an id — it must fail loudly
        // (caller logs the error + quoting hint) rather than half-parse.
        let parsed: Result<CategoriesConfig, _> =
            toml::from_str("[override]\nwaydroid.org.fdroid.fdroid = \"Android\"\n");
        assert!(parsed.is_err());
    }

    #[test]
    fn config_empty_is_default() {
        let cfg: CategoriesConfig = toml::from_str("").unwrap();
        assert!(cfg.overrides.is_empty());
        assert!(cfg.planet.is_empty());
    }
}
